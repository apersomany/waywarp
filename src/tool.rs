// The external programs Waywarp drives: ip, nft, and warp-cli.
use anyhow::{Context, Result, bail};
use nix::libc;
use nix::sys::signal::SigSet;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use tracing::debug;

// Runs a program to completion, feeding it `input`; stderr becomes the error message.
fn execute(program: &str, arguments: &[&str], input: Option<&str>) -> Result<String> {
    debug!(program, ?arguments, "running");
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = helper(&mut command)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .context("program stdin")?
            .write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// Helpers die with the thread that spawned them, and unblock the termination signals the
// supervisor blocks for its signal thread, so SIGTERM reaches them like any other process.
pub fn helper(command: &mut Command) -> &mut Command {
    // Only async-signal-safe calls run between fork and exec.
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

// Runs newline-separated `ip` commands for one family ("-4" or "-6") in a single process.
pub fn ip(family: &str, commands: &str) -> Result<()> {
    execute("ip", &[family, "-batch", "-"], Some(commands)).map(drop)
}

pub fn nft(script: &str) -> Result<()> {
    execute("nft", &["-f", "-"], Some(script)).map(drop)
}
