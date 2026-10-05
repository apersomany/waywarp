mod logging;

use crate::bridge::nat::Mode;
use crate::protocol::{Access, Failure, Progress, Status};
use crate::store::Name;
use crate::warp::State;
use anyhow::Result;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

pub use logging::{LogMode, init_logging};

pub(crate) const LIFECYCLE_TARGET: &str = "waywarp::lifecycle";

const FIELD_LABEL_WIDTH: usize = 9;

#[derive(Clone, Copy)]
enum Kind {
    Info,
    Warning,
    Error,
    Debug,
    Trace,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Info => "36",
            Self::Debug | Self::Trace => "2",
            Self::Warning => "33",
            Self::Error => "31",
        }
    }
}

fn color_enabled(terminal: bool, no_color: bool, term: Option<&str>) -> bool {
    terminal && !no_color && term != Some("dumb")
}

fn stderr_color() -> bool {
    color_enabled(
        io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        std::env::var("TERM").ok().as_deref(),
    )
}

// Remote relay names, geofeed cities, paths, and helper errors must not control the terminal.
fn safe_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

struct Renderer<W> {
    writer: W,
    color: bool,
}

impl<W: Write> Renderer<W> {
    fn accent(&mut self, color: &str, text: &str) -> io::Result<()> {
        if self.color {
            write!(self.writer, "\x1b[{color}m")?;
        }
        write!(self.writer, "{}", safe_text(text))?;
        if self.color {
            write!(self.writer, "\x1b[0m")?;
        }
        Ok(())
    }

    fn message(&mut self, kind: Kind, message: &str) -> io::Result<()> {
        self.accent(kind.color(), kind.label())?;
        let mut lines = message.split('\n');
        writeln!(
            self.writer,
            ": {}",
            safe_text(lines.next().unwrap_or_default())
        )?;
        for line in lines {
            writeln!(
                self.writer,
                "{:indent$}{}",
                "",
                safe_text(line),
                indent = kind.label().len() + 2,
            )?;
        }
        Ok(())
    }

    fn field_label(&mut self, label: &str) -> io::Result<()> {
        write!(
            self.writer,
            "{:<width$} ",
            safe_text(label),
            width = FIELD_LABEL_WIDTH + 1,
        )
    }

    fn field(&mut self, label: &str, value: impl fmt::Display) -> io::Result<()> {
        self.field_label(label)?;
        writeln!(self.writer, "{}", safe_text(&value.to_string()))
    }

    fn colored_field(
        &mut self,
        label: &str,
        value: impl fmt::Display,
        color: &str,
    ) -> io::Result<()> {
        self.accent(
            color,
            &format!("{label:<width$} {value}", width = FIELD_LABEL_WIDTH + 1),
        )?;
        writeln!(self.writer)
    }

    fn field_with_width(
        &mut self,
        label: &str,
        value: impl fmt::Display,
        label_width: usize,
    ) -> io::Result<()> {
        let label = if label.is_empty() {
            String::new()
        } else {
            format!("{}:", safe_text(label))
        };
        writeln!(
            self.writer,
            "  {label:<width$} {}",
            safe_text(&value.to_string()),
            width = label_width + 1,
        )
    }

    fn fields(&mut self, fields: &[(String, String)]) -> io::Result<()> {
        let width = fields
            .iter()
            .map(|(label, _)| safe_text(label).chars().count())
            .max()
            .unwrap_or_default()
            .max(FIELD_LABEL_WIDTH);
        for (label, value) in fields {
            self.field_with_width(label, value, width)?;
        }
        Ok(())
    }

    fn cause(&mut self, label: &str, message: &str) -> io::Result<()> {
        let width = safe_text(label).chars().count().max(FIELD_LABEL_WIDTH);
        let mut lines = message.split('\n');
        self.field_with_width(label, lines.next().unwrap_or_default(), width)?;
        for line in lines {
            self.field_with_width("", line, width)?;
        }
        Ok(())
    }

    fn failure(&mut self, kind: Kind, failure: &Failure) -> io::Result<()> {
        self.message(kind, &failure.message)?;
        for cause in &failure.causes {
            self.cause("caused by", cause)?;
        }
        Ok(())
    }

    fn status(&mut self, status: &Status) -> io::Result<()> {
        self.colored_field(
            "Instance",
            instance_identity(status.index, status.name.as_ref()),
            "36",
        )?;
        self.field_label("Status")?;
        self.accent(
            if status.healthy() { "32" } else { "33" },
            status_state(status.state),
        )?;
        if !status.matched {
            self.accent("31", " (location mismatched)")?;
        }
        writeln!(self.writer)?;
        match &status.access {
            Access::Proxy { listen } => {
                self.field("Access", format_args!("{listen} (proxy)"))?;
            }
            Access::Bridge { link, subnets, .. } => {
                self.field("Access", format_args!("{link} (bridge)"))?;
                let (v4, v6) = (subnets.v4(), subnets.v6());
                self.field("Link4", format_args!("{}/30 via {}", v4.host, v4.gateway))?;
                self.field("Link6", format_args!("{}/126 via {}", v6.host, v6.gateway))?;
            }
        }
        let locations = &status.locations;
        self.field(
            "Edge",
            if locations.edge.is_empty() {
                "Unavailable"
            } else {
                &locations.edge
            },
        )?;
        self.field("Bootstrap", status.relay.as_deref().unwrap_or("Direct"))?;
        for (label, geo) in [("Geo4", &locations.geo4), ("Geo6", &locations.geo6)] {
            self.field(
                label,
                geo.as_ref()
                    .map_or_else(|| "Unavailable".into(), ToString::to_string),
            )?;
        }
        for (label, probe) in [("Probe4", &locations.probe4), ("Probe6", &locations.probe6)] {
            self.field(
                label,
                match probe {
                    Some(probe) => format!("{} ({})", probe.address, probe.colo),
                    None => "Unavailable".into(),
                },
            )?;
        }
        if let Some(nat) = &status.nat {
            self.field("NAT", format_args!("{:?}", nat.mode))?;
            if nat.mode != Mode::Never || !nat.configuration_valid {
                self.field(
                    "SNAT4",
                    nat.v4.map_or_else(
                        || "blocked (no verified address)".into(),
                        |address| address.to_string(),
                    ),
                )?;
                self.field(
                    "SNAT6",
                    nat.v6.map_or_else(
                        || "blocked (no verified address)".into(),
                        |address| address.to_string(),
                    ),
                )?;
            }
            if nat.routed.is_empty() {
                self.field("Routed", "None")?;
            } else {
                for (index, route) in nat.routed.iter().enumerate() {
                    self.field(if index == 0 { "Routed" } else { "" }, route)?;
                }
            }
            if !nat.configuration_valid {
                self.field(
                    "Warning",
                    "registration unreadable; routed exemptions removed",
                )?;
                self.field("", "using last verified addresses")?;
            }
        }
        if status.rebootstraps > 0 {
            self.field(
                "Retries",
                format_args!(
                    "{} rebootstrap{}",
                    status.rebootstraps,
                    if status.rebootstraps == 1 { "" } else { "s" },
                ),
            )?;
        }
        Ok(())
    }

    fn statuses(&mut self, statuses: &[Status], json: bool) -> io::Result<()> {
        for (index, status) in statuses.iter().enumerate() {
            if json {
                serde_json::to_writer(&mut self.writer, status).map_err(|error| {
                    io::Error::new(error.io_error_kind().unwrap_or(io::ErrorKind::Other), error)
                })?;
                writeln!(self.writer)?;
            } else {
                if index > 0 {
                    writeln!(self.writer)?;
                }
                self.status(status)?;
            }
        }
        if statuses.is_empty() && !json {
            writeln!(self.writer, "no running instances")?;
        }
        Ok(())
    }
}

fn instance_identity(index: u8, name: Option<&Name>) -> String {
    match name {
        Some(name) => format!("{index} ({name})"),
        None => index.to_string(),
    }
}

fn status_state(state: State) -> &'static str {
    match state {
        State::Unknown => "Unknown",
        State::Disconnected => "Disconnected",
        State::Unable => "Unable to connect",
        State::Connecting => "Connecting",
        State::Degraded => "Degraded",
        State::Connected => "Connected",
    }
}

fn progress_message(progress: &Progress) -> String {
    match progress {
        Progress::Registering => "registering WARP".into(),
        Progress::StartingDaemon => "starting warp-svc".into(),
        Progress::Migrating => "moving tunnel to direct path".into(),
        Progress::CheckingLocations => "checking geo and probe".into(),
        Progress::Connecting { route } => format!("connecting {}", safe_text(route)),
        Progress::AttemptFailed { route, .. } => format!("failed to connect {}", safe_text(route)),
        Progress::RelayProbed { .. } => "relay probe succeeded".into(),
        Progress::RelayProbeFailed { .. } => "relay probe failed".into(),
    }
}

pub fn summary(status: &Status) -> String {
    safe_text(&format!(
        "Instance {}: {}{}, {}, edge {}",
        instance_identity(status.index, status.name.as_ref()),
        status.state,
        if status.matched {
            ""
        } else {
            " (location mismatched)"
        },
        status.access,
        if status.locations.edge.is_empty() {
            "unavailable"
        } else {
            &status.locations.edge
        },
    ))
}

#[derive(Debug)]
struct StdoutError(io::Error);

impl fmt::Display for StdoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("cannot write command output")
    }
}

impl std::error::Error for StdoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<StdoutError>()
        .is_some_and(|error| error.0.kind() == io::ErrorKind::BrokenPipe)
}

fn stdout(render: impl FnOnce(&mut Renderer<io::StdoutLock<'_>>) -> io::Result<()>) -> Result<()> {
    let stdout = io::stdout();
    let color = color_enabled(
        stdout.is_terminal(),
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        std::env::var("TERM").ok().as_deref(),
    );
    let mut renderer = Renderer {
        writer: stdout.lock(),
        color,
    };
    render(&mut renderer)
        .and_then(|()| renderer.writer.flush())
        .map_err(|error| StdoutError(error).into())
}

pub fn statuses(statuses: &[Status], json: bool) -> Result<()> {
    stdout(|renderer| renderer.statuses(statuses, json))
}

pub fn ready(status: &Status, log: Option<&Path>) -> Result<()> {
    stdout(|renderer| {
        renderer.status(status)?;
        if let Some(log) = log {
            renderer.field("Log", log.display())?;
        }
        Ok(())
    })
}

pub fn stopped(index: u8) {
    tracing::info!(target: LIFECYCLE_TARGET, instance = index, "stopped");
}

pub fn imported(index: u8, source: &Path) {
    tracing::info!(target: LIFECYCLE_TARGET, instance = index,
        from = %safe_text(&source.display().to_string()), "imported registration");
}

pub fn progress(index: u8, name: Option<&Name>, progress: &Progress) {
    let instance_name = name.map(ToString::to_string);
    let message = progress_message(progress);
    match progress {
        Progress::AttemptFailed { failure, .. } => {
            tracing::warn!(target: LIFECYCLE_TARGET, instance = index,
                instance_name = instance_name.as_deref(),
                error = %safe_text(&failure.message), causes = ?failure.causes, "{message}")
        }
        Progress::RelayProbed {
            relay,
            millis,
            colo,
        } => {
            tracing::debug!(target: LIFECYCLE_TARGET, instance = index,
                instance_name = instance_name.as_deref(),
                relay = %safe_text(relay), millis, colo = ?colo, "{message}")
        }
        Progress::RelayProbeFailed { relay, failure } => {
            tracing::debug!(target: LIFECYCLE_TARGET, instance = index,
                instance_name = instance_name.as_deref(),
                relay = %safe_text(relay), error = %safe_text(&failure.message),
                causes = ?failure.causes, "{message}")
        }
        _ => tracing::info!(target: LIFECYCLE_TARGET, instance = index,
            instance_name = instance_name.as_deref(), "{message}"),
    }
}

pub fn clap_error(error: &clap::Error) -> io::Result<()> {
    error.print()?;
    // Raw Clap errors omit the newline that parser-generated diagnostics already include.
    if !error.render().to_string().ends_with('\n') {
        if error.use_stderr() {
            writeln!(io::stderr().lock())?;
        } else {
            writeln!(io::stdout().lock())?;
        }
    }
    Ok(())
}

pub fn error(error: &anyhow::Error) -> io::Result<()> {
    Renderer {
        writer: io::stderr().lock(),
        color: stderr_color(),
    }
    .failure(Kind::Error, &Failure::from(error))
}

#[cfg(test)]
mod tests;
