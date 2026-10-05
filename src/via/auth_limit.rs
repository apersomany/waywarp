// Mudfish limits account logins, not UDP packets or nodes. Serialize and pace every SOCKS5
// authentication in a user's store, across threads, instances, and supervisor restarts. The
// runtime file contains only a monotonic timestamp; no account identifiers or credentials.
use super::transport::check_cancelled;
use crate::dataplane::Observer;
use anyhow::{Context, Result, bail};
use nix::libc;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

// At most ten authentication attempts per minute, below Mudfish's reported 15–20 limit.
const INTERVAL: Duration = Duration::from_secs(6);
const LOCK_RETRY: Duration = Duration::from_millis(50);

pub(super) fn run<T>(
    path: &Path,
    observer: &Observer,
    authenticate: impl FnOnce() -> Result<T>,
) -> Result<T> {
    paced(path, INTERVAL, observer, authenticate)
}

fn now() -> Result<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_BOOTTIME also counts suspend time. Runtime directories disappear at reboot.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error()).context("reading the Mudfish pacing clock");
    }
    Ok(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

fn stamp(file: &mut File) -> Result<()> {
    let timestamp = now()?.as_nanos().to_string();
    file.seek(SeekFrom::Start(0))?;
    file.write_all(timestamp.as_bytes())?;
    file.set_len(timestamp.len() as u64)?;
    file.flush()?;
    Ok(())
}

fn paced<T>(
    path: &Path,
    interval: Duration,
    observer: &Observer,
    authenticate: impl FnOnce() -> Result<T>,
) -> Result<T> {
    check_cancelled(observer)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("opening the Mudfish authentication limiter")?;
    // Keep the lock through the exchange, so a slow login cannot overlap the next one.
    // Every caller opens its own descriptor: flock serializes threads as well as processes.
    loop {
        check_cancelled(observer)?;
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                observer.wait_stopped(LOCK_RETRY);
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("locking the Mudfish authentication limiter");
            }
        }
    }
    if file.metadata()?.len() > 32 {
        bail!("invalid Mudfish authentication timestamp");
    }
    let mut previous = String::new();
    file.read_to_string(&mut previous)?;
    if !previous.is_empty() {
        let nanos = previous
            .parse::<u128>()
            .ok()
            .filter(|value| *value <= u64::MAX as u128 * 1_000_000_000)
            .context("invalid Mudfish authentication timestamp")?;
        let previous = Duration::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        );
        let elapsed = now()?.checked_sub(previous).unwrap_or_default();
        if let Some(wait) = interval.checked_sub(elapsed) {
            observer.wait_stopped(wait);
        }
    }
    check_cancelled(observer)?;
    // Count failures too, and retain a reservation if the process dies during authentication.
    stamp(&mut file)?;
    let result = authenticate();
    stamp(&mut file)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDirectory;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Instant;

    struct Directory(TempDirectory);

    impl Directory {
        fn new() -> Self {
            Self(TempDirectory::new("auth"))
        }
        fn file(&self) -> std::path::PathBuf {
            self.0.path.join("limit")
        }
    }

    #[test]
    fn cancellation_during_pacing_releases_the_lock_without_reserving_a_login() {
        let directory = Directory::new();
        let path = directory.file();
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        stamp(&mut file).unwrap();
        let timestamp = std::fs::read(&path).unwrap();
        let observer = Observer::default();
        let cancelled = observer.clone();
        let (finished, result) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let authentication: Result<()> =
                paced(&path, Duration::from_secs(3), &cancelled, || {
                    bail!("authentication must not start")
                });
            finished.send(authentication).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match file.try_lock() {
                Err(TryLockError::WouldBlock) => break,
                Ok(()) => file.unlock().unwrap(),
                Err(error) => panic!("{error}"),
            }
            assert!(Instant::now() < deadline, "pacing did not acquire the lock");
            thread::sleep(Duration::from_millis(1));
        }
        observer.stop();
        let completed = result.recv_timeout(Duration::from_secs(1));
        worker.join().unwrap();
        assert_eq!(
            completed.unwrap().unwrap_err().to_string(),
            "relay operation was cancelled"
        );
        // Other tests can fork while the worker holds the descriptor, before CLOEXEC closes it.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {}
                Err(error) => panic!("{error}"),
            }
            assert!(
                Instant::now() < deadline,
                "cancellation did not release the lock"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(std::fs::read(directory.file()).unwrap(), timestamp);
    }

    #[test]
    fn cancellation_before_authentication_does_not_create_the_limiter() {
        let directory = Directory::new();
        let observer = Observer::default();
        observer.stop();
        assert!(
            run::<()>(&directory.file(), &observer, || panic!(
                "must not authenticate"
            ))
            .is_err()
        );
        assert!(!directory.file().exists());
    }

    #[test]
    fn independent_callers_share_spacing() {
        let directory = Directory::new();
        let barrier = Arc::new(Barrier::new(4));
        let starts = Arc::new(Mutex::new(Vec::new()));
        thread::scope(|scope| {
            for _ in 0..4 {
                let path = directory.file();
                let barrier = Arc::clone(&barrier);
                let starts = Arc::clone(&starts);
                scope.spawn(move || {
                    barrier.wait();
                    paced(
                        &path,
                        Duration::from_millis(20),
                        &Observer::default(),
                        || {
                            starts.lock().unwrap().push(Instant::now());
                            Ok(())
                        },
                    )
                    .unwrap();
                });
            }
        });
        let mut starts = starts.lock().unwrap();
        starts.sort();
        assert_eq!(starts.len(), 4);
        for pair in starts.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= Duration::from_millis(20));
        }
    }

    #[test]
    fn process_worker() {
        let Some(path) = std::env::var_os("WAYWARP_TEST_AUTH_LIMIT") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        paced(
            &path,
            Duration::from_millis(20),
            &Observer::default(),
            || {
                let mut samples = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.with_extension("samples"))?;
                writeln!(samples, "{}", now()?.as_nanos())?;
                Ok(())
            },
        )
        .unwrap();
    }

    #[test]
    fn separate_processes_share_spacing() {
        let directory = Directory::new();
        let mut children = Vec::new();
        for _ in 0..3 {
            children.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "via::auth_limit::tests::process_worker"])
                    .env("WAYWARP_TEST_AUTH_LIMIT", directory.file())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let samples: Vec<u128> =
            std::fs::read_to_string(directory.file().with_extension("samples"))
                .unwrap()
                .lines()
                .map(|line| line.parse().unwrap())
                .collect();
        assert_eq!(samples.len(), 3);
        for pair in samples.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_millis(20).as_nanos());
        }
    }

    #[test]
    fn failed_authentication_is_paced_too() {
        let directory = Directory::new();
        let interval = Duration::from_millis(20);
        let mut failed_at = None;
        let result: Result<()> = paced(&directory.file(), interval, &Observer::default(), || {
            failed_at = Some(Instant::now());
            bail!("rejected")
        });
        assert!(result.is_err());
        paced(&directory.file(), interval, &Observer::default(), || {
            assert!(failed_at.unwrap().elapsed() >= interval);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn malformed_state_fails_closed() {
        let directory = Directory::new();
        std::fs::write(directory.file(), "not a timestamp").unwrap();
        assert!(
            paced::<()>(
                &directory.file(),
                Duration::ZERO,
                &Observer::default(),
                || panic!("must not authenticate")
            )
            .is_err()
        );
    }

    #[test]
    fn symlinks_are_not_followed() {
        let directory = Directory::new();
        let target = directory.0.path.join("target");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, directory.file()).unwrap();
        assert!(
            paced::<()>(
                &directory.file(),
                Duration::ZERO,
                &Observer::default(),
                || panic!("must not authenticate")
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }
}
