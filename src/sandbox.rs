// The private mount and network namespaces that confine each warp-svc.
use crate::store::Instance;
use crate::tool;
use anyhow::{Context, Result, bail};
use nix::libc;
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, setns, unshare};
use nix::unistd::{Gid, Uid};
use std::fs::{self, File, OpenOptions};
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

// The TUN that carries warp-svc's traffic out of the private namespace; the name never reaches
// the host.
const TUN: &str = "tun";
const TUN_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 79, 0, 2);

fn bind(source: &Path, target: &Path) -> Result<()> {
    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .with_context(|| {
        format!(
            "bind-mounting {} onto {}",
            source.display(),
            target.display()
        )
    })
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
}

fn on_path(name: &str) -> Result<PathBuf> {
    let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| executable(path))
        .with_context(|| format!("{name} is not on PATH"))?;
    Ok(fs::canonicalize(path)?)
}

// warp-svc runs /usr/sbin/ip and /usr/sbin/nft by absolute path, which NixOS lacks. An overlay
// adds them from PATH without hiding anything else in /usr.
fn provide_sbin(instance: &Instance) -> Result<()> {
    const TOOLS: [&str; 2] = ["ip", "nft"];
    let sbin = Path::new("/usr/sbin");
    if TOOLS.iter().all(|tool| executable(&sbin.join(tool))) {
        return Ok(());
    }
    let layer = instance.runtime().join("usr");
    if layer.exists() {
        fs::remove_dir_all(&layer)?;
    }
    fs::create_dir_all(layer.join("sbin"))?;
    for tool in TOOLS {
        std::os::unix::fs::symlink(on_path(tool)?, layer.join("sbin").join(tool))?;
    }
    let options = format!("lowerdir={}:/usr", layer.display());
    mount(
        Some("overlay"),
        "/usr",
        Some("overlay"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(options.as_str()),
    )
    .context("adding ip and nft to /usr/sbin")
}

// Must run while the process is single-threaded: gives warp-svc private copies of every path it
// writes. Rootless instances first map the caller to root inside a new user namespace.
pub fn isolate(instance: &Instance) -> Result<()> {
    let mounts = [
        (instance.registration(), "/var/lib/cloudflare-warp"),
        (instance.warp_logs(), "/var/log/cloudflare-warp"),
        (instance.daemon_socket(), "/run/cloudflare-warp"),
        (instance.resolv_conf(), "/etc/resolv.conf"),
    ];
    // Bind mounts need their targets; WARP's own directories exist once its package or service
    // has run, and root can create them without disturbing anything.
    for (_, target) in &mounts[..3] {
        let target = Path::new(target);
        if !target.exists() {
            if !Uid::effective().is_root() {
                bail!(
                    "{} does not exist; create it once as root, or start the WARP service once",
                    target.display()
                );
            }
            fs::create_dir_all(target)?;
            fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
        }
    }
    let uid = Uid::effective();
    if !uid.is_root() {
        let gid = Gid::effective();
        unshare(CloneFlags::CLONE_NEWUSER).context("creating a user namespace")?;
        fs::write("/proc/self/setgroups", "deny")?;
        fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
        fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
    }
    unshare(CloneFlags::CLONE_NEWNS).context("creating a mount namespace")?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )?;
    for (source, target) in mounts {
        bind(&source, Path::new(target))?;
    }
    hide_nscd()?;
    provide_sbin(instance)
}

// glibc hands name lookups to a running nscd, which resolves them in the host's network
// namespace. warp-svc must resolve through its own DNS proxy, named in its private resolv.conf, or
// its connectivity check fails, so an empty directory hides the socket.
fn hide_nscd() -> Result<()> {
    let nscd = Path::new("/run/nscd");
    if !nscd.is_dir() {
        return Ok(());
    }
    mount(
        Some("tmpfs"),
        nscd,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some("mode=0755"),
    )
    .context("hiding the host's nscd")
}

// Runs inside the private namespace: redirects TCP routed to the emulated physical uplink.
// Traffic warp-svc routes through CloudflareWARP must stay in its tunnel, including connector
// control traffic. The listener's connections are forwarded out of the host's physical interface.
pub fn redirect_tcp() -> Result<TcpListener> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    tool::nft(&format!(
        "table ip waywarp_redirect {{
            chain output {{
                type nat hook output priority dstnat; policy accept;
                oifname \"{TUN}\" meta l4proto tcp ip daddr != 127.0.0.0/8 redirect to :{port}
            }}
        }}"
    ))
    .context("redirecting TCP")?;
    Ok(listener)
}

// Runs inside the private namespace; the TUN disappears when the returned file closes.
pub fn tun() -> Result<File> {
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/net/tun")
        .context("opening /dev/net/tun")?;
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(TUN.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    request.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
    // TUNSETIFF reads and writes only the ifreq it is given.
    if unsafe { libc::ioctl(device.as_raw_fd(), libc::TUNSETIFF as _, &mut request) } < 0 {
        return Err(std::io::Error::last_os_error()).context("creating the TUN");
    }
    tool::ip(
        "-4",
        &format!(
            "link set lo up
            address add {TUN_ADDRESS}/32 dev {TUN}
            link set {TUN} up
            route add default dev {TUN} src {TUN_ADDRESS}"
        ),
    )?;
    Ok(device)
}

// Network namespaces are per thread, and a rootless process cannot switch back to the host stack,
// so each private task runs on its own short-lived thread that enters and never leaves. Links,
// sockets, and processes created there keep the namespace. An owned anchor stays inside so
// the namespace has a path that tools such as `ip` can name.
#[derive(Clone)]
pub struct Private(Arc<Namespace>);

struct Namespace {
    descriptor: OwnedFd,
    path: PathBuf,
    lifetime: Option<mpsc::Sender<()>>,
    owner: Option<thread::JoinHandle<()>>,
}

impl Drop for Namespace {
    fn drop(&mut self) {
        self.lifetime.take();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

impl Private {
    pub fn create() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (alive, lifetime) = mpsc::channel();
        let owner = thread::Builder::new()
            .name("namespace".into())
            .spawn(move || {
                let entered = unshare(CloneFlags::CLONE_NEWNET)
                    .context("creating a network namespace")
                    .and_then(|()| {
                        let descriptor = File::open("/proc/thread-self/ns/net")?;
                        Ok((descriptor, nix::unistd::gettid()))
                    });
                let anchored = entered.is_ok();
                let _ = sender.send(entered);
                if anchored {
                    let _ = lifetime.recv();
                }
            })?;
        let (descriptor, thread) = receiver.recv()??;
        Ok(Self(Arc::new(Namespace {
            descriptor: descriptor.into(),
            path: format!("/proc/{}/task/{thread}/ns/net", std::process::id()).into(),
            lifetime: Some(alive),
            owner: Some(owner),
        })))
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    fn enter(&self) -> Result<()> {
        setns(&self.0.descriptor, CloneFlags::CLONE_NEWNET)
            .context("entering the private network namespace")
    }

    // For long-lived work such as supervising warp-svc, whose parent-death signal follows this thread.
    pub fn spawn(
        &self,
        name: &str,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<thread::JoinHandle<Result<()>>> {
        let private = self.clone();
        Ok(thread::Builder::new().name(name.into()).spawn(move || {
            private.enter()?;
            task();
            Ok(())
        })?)
    }

    pub fn run<T: Send>(&self, task: impl FnOnce() -> Result<T> + Send) -> Result<T> {
        thread::scope(|scope| {
            scope
                .spawn(|| {
                    self.enter()?;
                    task()
                })
                .join()
                .map_err(|_| anyhow::anyhow!("private namespace task panicked"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_namespace_handle_releases_and_joins_the_anchor() {
        let (alive, lifetime) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let owner = thread::spawn(move || {
            let _ = lifetime.recv();
            finished.send(()).unwrap();
        });
        let private = Private(Arc::new(Namespace {
            descriptor: File::open("/dev/null").unwrap().into(),
            path: PathBuf::new(),
            lifetime: Some(alive),
            owner: Some(owner),
        }));
        let last = private.clone();
        drop(private);
        assert!(done.try_recv().is_err());
        drop(last);
        done.try_recv().unwrap();
    }
}
