mod bridge;
mod cli;
mod client;
mod dataplane;
mod http;
mod ipc;
mod location;
mod notify;
mod protocol;
mod sandbox;
mod store;
mod supervise;
mod text;
mod tool;
mod via;
mod warp;

use clap::Parser;
use cli::Command;
use std::io::IsTerminal;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

// WAYWARP_LOG takes comma-separated directives such as `debug` or `info,waywarp::dataplane=trace`.
// An invalid value falls back to the default rather than hiding every message.
fn filter(default: &str) -> Targets {
    std::env::var("WAYWARP_LOG")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| default.parse().expect("valid default filter"))
}

// Commands log warnings to the terminal; supervisors log everything to the instance log, or to
// journald through stderr in the foreground.
fn log(supervisor: bool) {
    let terminal = std::io::stderr().is_terminal();
    let layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(terminal)
        .with_target(supervisor)
        .with_thread_names(supervisor);
    let filter = filter(if supervisor { "info" } else { "warn" });
    let registry = tracing_subscriber::registry();
    // Terminals and journald add their own timestamps.
    if supervisor && !terminal {
        registry.with(layer.with_filter(filter)).init();
    } else {
        registry
            .with(layer.without_time().with_filter(filter))
            .init();
    }
}

fn main() -> anyhow::Result<()> {
    // `__supervise` is hidden: `up` starts it with the plan on stdin.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "__supervise")
    {
        log(true);
        return supervise::detached();
    }
    let command = cli::Cli::parse().command;
    let foreground = matches!(&command, Command::Up(access) if access.common().foreground);
    log(foreground);
    match command {
        Command::Up(access) => client::up(*access),
        Command::Down { instance } => client::down(&instance),
        Command::Status { instance, json } => client::status(instance.as_ref(), json),
        Command::Import {
            instance,
            from,
            replace,
        } => client::import_registration(&instance, &from, replace),
        Command::WarpCli {
            instance,
            arguments,
        } => client::warp_cli(&instance, arguments),
    }
}
