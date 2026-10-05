// The external programs Waywarp drives: ip, nft, and warp-cli.
use anyhow::{Context, Result, anyhow};
use nix::libc;
use nix::sys::signal::SigSet;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;

const HELPER_DEADLINE: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;

fn execute(program: &str, arguments: &[&str], input: Option<&str>) -> Result<String> {
    execute_with_deadline(program, arguments, input, HELPER_DEADLINE, || false)
}

enum PipeResult {
    Stdout(std::io::Result<Vec<u8>>),
    Stderr(std::io::Result<Vec<u8>>),
    Stdin(std::io::Result<()>),
}

struct InternalChild {
    child: Child,
    completed: bool,
}

impl Drop for InternalChild {
    fn drop(&mut self) {
        if !self.completed {
            terminate(&mut self.child);
        }
    }
}

fn execute_with_deadline(
    program: &str,
    arguments: &[&str],
    input: Option<&str>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    if cancelled() {
        return Err(anyhow!("{program} was cancelled"));
    }
    debug!(program, ?arguments, "running");
    let deadline = Instant::now() + timeout;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    thread::scope(|scope| {
        // This guard drops before the scope joins pipe workers, unblocking them on every error.
        let mut process = InternalChild {
            child: helper(&mut command)
                .spawn()
                .with_context(|| format!("running {program}"))?,
            completed: false,
        };
        let stdout = process.child.stdout.take().context("program stdout")?;
        let stderr = process.child.stderr.take().context("program stderr")?;
        let (sender, results) = std::sync::mpsc::channel();
        let output_sender = sender.clone();
        scope.spawn(move || {
            let _ = output_sender.send(PipeResult::Stdout(read_pipe(stdout)));
        });
        let error_sender = sender.clone();
        scope.spawn(move || {
            let _ = error_sender.send(PipeResult::Stderr(read_pipe(stderr)));
        });
        if let Some(input) = input {
            let mut stdin = process.child.stdin.take().context("program stdin")?;
            let input_sender = sender.clone();
            scope.spawn(move || {
                let _ = input_sender.send(PipeResult::Stdin(stdin.write_all(input.as_bytes())));
            });
        }
        drop(sender);
        let mut remaining = 2 + usize::from(input.is_some());
        let (mut status, mut output, mut error, mut write_error) =
            (None, Vec::new(), Vec::new(), None);
        while status.is_none() || remaining > 0 {
            if cancelled() {
                return Err(anyhow!("{program} was cancelled"));
            }
            if status.is_none() {
                status = process.child.try_wait()?;
            }
            if status.is_some() && remaining == 0 {
                break;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(anyhow!("{program} {} timed out", arguments.join(" ")));
            }
            match results.recv_timeout(wait.min(Duration::from_millis(10))) {
                Ok(result) => {
                    remaining -= 1;
                    match result {
                        PipeResult::Stdout(result) => {
                            output = result.context("reading helper stdout")?
                        }
                        PipeResult::Stderr(result) => {
                            error = result.context("reading helper stderr")?
                        }
                        PipeResult::Stdin(result) => write_error = result.err(),
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) if remaining == 0 => {
                    thread::sleep(wait.min(Duration::from_millis(10)))
                }
                Err(error) => return Err(error).context("reading helper pipes"),
            }
        }
        let status = status.unwrap();
        if !status.success() {
            return Err(command_failure(program, arguments, status, &error));
        }
        if let Some(error) = write_error {
            return Err(error).with_context(|| format!("writing to {program}"));
        }
        process.completed = true;
        Ok(String::from_utf8_lossy(&output).into_owned())
    })
}

fn read_pipe(pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.take(MAX_OUTPUT + 1).read_to_end(&mut output)?;
    if output.len() as u64 > MAX_OUTPUT {
        return Err(std::io::Error::other("helper output exceeds 16 MiB"));
    }
    Ok(output)
}

fn terminate(child: &mut Child) {
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn command_failure(
    program: &str,
    arguments: &[&str],
    status: std::process::ExitStatus,
    stderr: &[u8],
) -> anyhow::Error {
    let message = format!("{program} {} failed ({status})", arguments.join(" "));
    let detail = String::from_utf8_lossy(stderr);
    if detail.trim().is_empty() {
        anyhow!(message)
    } else {
        anyhow!(detail.trim().to_owned()).context(message)
    }
}

// Helpers die with the thread that spawned them, and unblock the termination signals the
// supervisor blocks for its signal thread, so SIGTERM reaches them like any other process.
pub fn helper(command: &mut Command) -> &mut Command {
    unsafe {
        command.pre_exec(|| {
            SigSet::empty().thread_set_mask()?;
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

pub fn run(program: &str, arguments: &[&str]) -> Result<String> {
    execute(program, arguments, None)
}

pub fn run_cancellable(
    program: &str,
    arguments: &[&str],
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    execute_with_deadline(program, arguments, None, HELPER_DEADLINE, cancelled)
}

pub fn ip(family: &str, commands: &str) -> Result<()> {
    execute("ip", &[family, "-batch", "-"], Some(commands)).map(drop)
}

pub fn nft(script: &str) -> Result<()> {
    execute("nft", &["-f", "-"], Some(script)).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn helper_failure_preserves_exit_status_and_multiline_stderr() {
        let error = command_failure(
            "warp-cli",
            &["connect"],
            std::process::ExitStatus::from_raw(7 << 8),
            b"first line\nsecond line\n",
        );
        assert_eq!(
            error.to_string(),
            "warp-cli connect failed (exit status: 7)"
        );
        assert_eq!(error.root_cause().to_string(), "first line\nsecond line");
    }

    #[test]
    fn helper_failure_without_stderr_still_explains_the_exit() {
        let error = command_failure(
            "ip",
            &["link"],
            std::process::ExitStatus::from_raw(9),
            b"\n",
        );
        assert!(error.to_string().contains("signal: 9"));
        assert_eq!(error.chain().count(), 1);
    }

    #[test]
    fn bounded_execute_times_out_and_kills_group() {
        for script in ["sleep 10 & wait", "sleep 10 & exit 0"] {
            let started = Instant::now();
            let error = execute_with_deadline(
                "sh",
                &["-c", script],
                None,
                Duration::from_millis(30),
                || false,
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"));
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn cancellation_reaps_helpers_before_joining_their_pipes() {
        let directory = crate::test_support::TempDirectory::new("helper-cancel");
        for (index, script) in [
            "printf ready >\"$1\"; sleep 10 & wait",
            "printf ready >\"$1\"; sleep 10 & exit 0",
            "exec 3<&0; printf ready >\"$1\"; sleep 10 <&3 >/dev/null 2>&1 & exit 0",
        ]
        .into_iter()
        .enumerate()
        {
            let ready = directory.path.join(index.to_string());
            let input = "x".repeat(2_000_000);
            let started = Instant::now();
            let error = execute_with_deadline(
                "sh",
                &["-c", script, "sh", ready.to_str().unwrap()],
                Some(&input),
                Duration::from_secs(5),
                || ready.exists(),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), "sh was cancelled");
            assert!(
                ready.exists(),
                "cancellation must occur after helper startup"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
        assert_eq!(
            run_cancellable("waywarp-must-not-execute", &[], || true)
                .unwrap_err()
                .to_string(),
            "waywarp-must-not-execute was cancelled"
        );
    }

    #[test]
    fn bounded_execute_drains_large_input_and_output() {
        let input = "x".repeat(2_000_000);
        let output = execute_with_deadline(
            "sh",
            &[
                "-c",
                "head -c 2000000 /dev/zero; head -c 2000000 /dev/zero >&2; cat >/dev/null",
            ],
            Some(&input),
            Duration::from_secs(5),
            || false,
        )
        .unwrap();
        assert_eq!(output.len(), 2_000_000);
    }

    #[test]
    fn pipe_failure_kills_the_process_before_joining_readers() {
        let started = Instant::now();
        let error = execute_with_deadline(
            "sh",
            &["-c", "head -c 16777217 /dev/zero; sleep 10"],
            None,
            Duration::from_secs(5),
            || false,
        )
        .unwrap_err();
        assert!(error.root_cause().to_string().contains("output exceeds"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn rejected_input_preserves_the_commands_stderr() {
        let input = "x".repeat(2_000_000);
        let error = execute_with_deadline(
            "sh",
            &["-c", "printf 'first line\\nsecond line\\n' >&2; exit 7"],
            Some(&input),
            Duration::from_secs(5),
            || false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("exit status: 7"));
        assert_eq!(error.root_cause().to_string(), "first line\nsecond line");
    }

    #[test]
    fn deadline_also_covers_stdin_after_the_parent_exits() {
        let input = "x".repeat(2_000_000);
        let started = Instant::now();
        let error = execute_with_deadline(
            "sh",
            &["-c", "exec 3<&0; sleep 10 <&3 >/dev/null 2>&1 & exit 0"],
            Some(&input),
            Duration::from_millis(30),
            || false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
