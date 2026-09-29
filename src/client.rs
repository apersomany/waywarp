// Commands that start instances or talk to their supervisors.
use crate::bridge::{self, Subnets};
use crate::cli::{self, Up};
use crate::ipc::Channel;
use crate::notify::Notifier;
use crate::protocol::{Access, Event, Plan, Request, Response, Status};
use crate::store::{Instance, Lock, Selector, Store};
use crate::supervise::{self, Reporter};
use crate::via::interface::Interface;
use crate::via::{self, Credentials};
use crate::warp::State;
use anyhow::{Context, Result, bail};
use nix::unistd::Uid;
use std::io::IsTerminal;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::info;

const DEFAULT_EDGE: Ipv4Addr = Ipv4Addr::new(162, 159, 198, 1);

// Checks access-specific options on the host, before any instance state changes. A proxy listener
// is bound here so an address conflict is reported before setup begins.
fn access(access: &cli::Access, index: u8) -> Result<(Access, Option<TcpListener>)> {
    match *access {
        cli::Access::Bridge {
            subnet4, subnet6, ..
        } => {
            if !Uid::effective().is_root() {
                bail!("bridge access requires root privileges; run with sudo");
            }
            let link = bridge::link(index);
            if Path::new("/sys/class/net").join(&link).exists() {
                bail!("host link {link} already exists");
            }
            let subnets = Subnets::new(index, subnet4, subnet6);
            subnets.check_routes()?;
            Ok((Access::Bridge { link, subnets }, None))
        }
        cli::Access::Proxy { listen, .. } => {
            let listen = listen.unwrap_or(SocketAddrV4::new(
                Ipv4Addr::LOCALHOST,
                1080 + u16::from(index),
            ));
            let listener = TcpListener::bind(listen).with_context(|| {
                format!("cannot listen on {listen}; choose another address with --listen")
            })?;
            Ok((Access::Proxy { listen }, Some(listener)))
        }
    }
}

pub fn up(requested: cli::Access) -> Result<()> {
    // Read first, while the process is single-threaded, so no helper inherits them.
    let credentials = Credentials::take(&requested.common().via)?;
    let notifier = Notifier::take();
    let common: &Up = requested.common();
    via::validate(&common.via)?;
    let store = Store::current()?;
    let instance = store.resolve(&common.instance)?;
    let lock = instance.lock()?;
    let (access, listener) = access(&requested, instance.index)?;
    if let Some(name) = &common.name {
        store.assign(&instance, name)?;
    }
    let registration_edge = instance.registration_edge(common.edge_port)?;
    let edge = SocketAddrV4::new(
        common.edge.or(registration_edge).unwrap_or(DEFAULT_EDGE),
        common.edge_port,
    );
    let plan = Plan {
        name: instance.name()?,
        instance: instance.clone(),
        access,
        interface: Interface::new(common.interface.clone())?,
        edge,
        redirect_tcp: registration_edge.is_some(),
        location: common.location.clone().unwrap_or_default(),
        via: common.via.clone(),
        credentials,
        mudfish_port: common.mudfish_port,
        rebootstrap: !common.no_rebootstrap,
    };
    instance.prepare()?;
    if common.foreground {
        return supervise::run(plan, lock, listener, Reporter::Foreground(notifier)).with_context(
            || {
                format!(
                    "instance {} failed; see {}",
                    instance.index,
                    instance.log().display()
                )
            },
        );
    }
    let status = detach(&plan, lock, listener)?;
    println!("{status}");
    Ok(())
}

// Starts a supervisor in its own session, hands it the plan, the lock, and any listener, and
// waits until it reports the instance up.
fn detach(plan: &Plan, lock: Lock, listener: Option<TcpListener>) -> Result<Status> {
    let (parent, setup) = Channel::pair()?;
    // Each run starts a fresh, owner-only log describing the current or most recent instance.
    let log = std::fs::File::create(plan.instance.log())?;
    log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__supervise")
        .stdin(Stdio::from(OwnedFd::from(setup)))
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // A new session detaches the supervisor from the terminal and its signals.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(drop)
                .map_err(std::io::Error::from)
        });
    }
    command.spawn().context("starting the supervisor")?;
    let lock = OwnedFd::from(lock);
    let mut descriptors = vec![lock.as_fd()];
    descriptors.extend(listener.as_ref().map(AsFd::as_fd));
    parent.send(plan, &descriptors)?;
    let interactive = std::io::stderr().is_terminal();
    loop {
        let event = parent
            .receive::<Event>()?
            .with_context(|| {
                format!(
                    "supervisor exited during setup; see {}",
                    plan.instance.log().display()
                )
            })?
            .0;
        match event {
            Event::Progress(message) if interactive => eprintln!("{message}…"),
            Event::Progress(message) => info!("{message}"),
            Event::Up(status) => return Ok(*status),
            Event::Failed(reason) => bail!("{reason}"),
        }
    }
}

pub fn import_registration(selector: &Selector, source: &Path, replace: bool) -> Result<()> {
    let instance = Store::current()?.resolve(selector)?;
    let _lock = instance.lock()?;
    instance.import_registration(source, replace)?;
    println!(
        "{}: imported registration from {}",
        instance.index,
        source.display()
    );
    Ok(())
}

fn request(
    instance: &Instance,
    request: &Request,
    descriptors: &[std::os::fd::BorrowedFd<'_>],
) -> Result<Channel> {
    let channel = Channel::connect(&instance.control()).map_err(|_| {
        let hint = if Uid::effective().is_root() {
            ""
        } else {
            " (root-owned instances are only visible with sudo)"
        };
        anyhow::anyhow!("instance {} is not running{hint}", instance.index)
    })?;
    channel.send(request, descriptors)?;
    Ok(channel)
}

pub fn down(selector: &Selector) -> Result<()> {
    let instance = Store::current()?.resolve(selector)?;
    let channel = request(&instance, &Request::Stop, &[])?;
    channel.set_timeout(Some(Duration::from_secs(15)))?;
    // The supervisor closes the channel by exiting once shutdown completes.
    match channel.receive::<Response>() {
        Ok(None) => {}
        Ok(Some((Response::Failed(reason), _))) => bail!("{reason}"),
        Ok(Some(_)) => bail!("unexpected supervisor response"),
        Err(error) => return Err(error.context("instance did not stop within 15 s")),
    }
    println!("{} down", instance.index);
    Ok(())
}

fn status_of(instance: &Instance) -> Result<Status> {
    let channel = request(instance, &Request::Status, &[])?;
    channel.set_timeout(Some(Duration::from_secs(5)))?;
    match channel.expect::<Response>()?.0 {
        Response::Status(status) => Ok(*status),
        Response::Failed(reason) => bail!("{reason}"),
        _ => bail!("unexpected supervisor response"),
    }
}

pub fn status(selector: Option<&Selector>, json: bool) -> Result<()> {
    let store = Store::current()?;
    let instances = match selector {
        Some(selector) => vec![store.resolve(selector)?],
        None => store.running()?,
    };
    let mut healthy = true;
    let mut shown = 0;
    for instance in instances {
        let status = match status_of(&instance) {
            Ok(status) => status,
            Err(error) if selector.is_some() => return Err(error),
            // An instance that stopped since the listing is simply no longer running.
            Err(_) => continue,
        };
        healthy &= status.state == State::Connected && status.matched;
        shown += 1;
        if json {
            println!("{}", serde_json::to_string(&status)?);
        } else {
            println!("{status}");
        }
    }
    if shown == 0 && !json {
        println!("no running instances");
    }
    if !healthy {
        std::process::exit(1);
    }
    Ok(())
}

pub fn warp_cli(selector: &Selector, arguments: Vec<String>) -> Result<()> {
    let instance = Store::current()?.resolve(selector)?;
    let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
    let channel = request(
        &instance,
        &Request::WarpCli(arguments),
        &[stdout.as_fd(), stderr.as_fd()],
    )?;
    match channel.expect::<Response>()?.0 {
        Response::Exit(code) => std::process::exit(code),
        Response::Failed(reason) => bail!("{reason}"),
        _ => bail!("unexpected supervisor response"),
    }
}
