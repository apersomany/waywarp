mod bridge;
mod cli;
mod client;
mod dataplane;
mod http;
mod ipc;
mod location;
mod notify;
mod output;
mod protocol;
mod sandbox;
mod store;
mod supervise;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
mod text;
mod tool;
mod via;
mod warp;

use clap::Parser;
use cli::Command;
use output::LogMode;
use std::process::ExitCode;

fn run() -> anyhow::Result<ExitCode> {
    // `__supervise` is hidden: `up` starts it with the plan on stdin.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "__supervise")
    {
        output::init_logging(LogMode::Detached);
        return supervise::detached().map(|()| ExitCode::SUCCESS);
    }
    let command = cli::Cli::try_parse()?.command;
    let foreground = matches!(&command, Command::Up(access) if access.common().foreground);
    output::init_logging(if foreground {
        LogMode::Foreground
    } else {
        LogMode::Command
    });
    match command {
        Command::Up(access) => client::up(*access).map(|()| ExitCode::SUCCESS),
        Command::Down { instance } => client::down(&instance).map(|()| ExitCode::SUCCESS),
        Command::Status { instance, json } => client::status(instance.as_ref(), json),
        Command::Import {
            instance,
            from,
            replace,
        } => client::import_registration(&instance, &from, replace).map(|()| ExitCode::SUCCESS),
        Command::WarpCli {
            instance,
            arguments,
        } => client::warp_cli(&instance, arguments),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) if output::is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(error) = error.downcast_ref::<clap::Error>() {
                let _ = output::clap_error(error);
                ExitCode::from(error.exit_code() as u8)
            } else {
                let _ = output::error(&error);
                ExitCode::FAILURE
            }
        }
    }
}
