use super::{Kind, LIFECYCLE_TARGET, Renderer, stderr_color};
use std::fmt;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone, Copy)]
pub enum LogMode {
    Command,
    Foreground,
    Detached,
}

fn filter(value: Option<&str>, supervisor: bool) -> Targets {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            Targets::new()
                .with_default(if supervisor { Level::INFO } else { Level::WARN })
                .with_target(LIFECYCLE_TARGET, Level::INFO)
        })
}

pub fn init_logging(mode: LogMode) {
    let supervisor = !matches!(mode, LogMode::Command);
    let filter = filter(std::env::var("WAYWARP_LOG").ok().as_deref(), supervisor);
    let registry = tracing_subscriber::registry();
    let layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    match mode {
        LogMode::Detached => registry
            .with(
                layer
                    .with_ansi(false)
                    .with_thread_names(true)
                    .with_filter(filter),
            )
            .init(),
        LogMode::Command | LogMode::Foreground => registry
            .with(
                layer
                    .event_format(ConsoleFormat)
                    .with_ansi(stderr_color())
                    .with_filter(filter),
            )
            .init(),
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    values: Vec<(String, String)>,
}

impl Fields {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.values.push((field.name().into(), value));
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

struct ConsoleFormat;

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        fields
            .values
            .retain(|(label, _)| !matches!(label.as_str(), "instance" | "instance_name"));
        let metadata = event.metadata();
        let kind = match *metadata.level() {
            Level::ERROR => Kind::Error,
            Level::WARN => Kind::Warning,
            Level::INFO => Kind::Info,
            Level::DEBUG => Kind::Debug,
            Level::TRACE => Kind::Trace,
        };
        if matches!(kind, Kind::Debug | Kind::Trace) {
            fields.message = format!("{}: {}", metadata.target(), fields.message);
        }
        let mut renderer = Renderer {
            writer: Vec::new(),
            color: writer.has_ansi_escapes(),
        };
        renderer
            .message(kind, &fields.message)
            .map_err(|_| fmt::Error)?;
        renderer.fields(&fields.values).map_err(|_| fmt::Error)?;
        writer.write_str(std::str::from_utf8(&renderer.writer).map_err(|_| fmt::Error)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn render_events(events: impl FnOnce()) -> String {
        render_events_with_filter(LogMode::Foreground, Some("trace"), events)
    }

    fn render_events_with_filter(
        mode: LogMode,
        directive: Option<&str>,
        events: impl FnOnce(),
    ) -> String {
        let capture = Capture::default();
        let output = capture.clone();
        let registry = tracing_subscriber::registry();
        let layer = tracing_subscriber::fmt::layer().with_writer(move || output.clone());
        let filter = filter(directive, !matches!(mode, LogMode::Command));
        match mode {
            LogMode::Detached => tracing::subscriber::with_default(
                registry.with(
                    layer
                        .with_ansi(false)
                        .with_thread_names(true)
                        .without_time()
                        .with_filter(filter),
                ),
                events,
            ),
            LogMode::Command | LogMode::Foreground => tracing::subscriber::with_default(
                registry.with(
                    layer
                        .event_format(ConsoleFormat)
                        .with_ansi(false)
                        .with_filter(filter),
                ),
                events,
            ),
        }
        String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn diagnostics_use_the_same_renderer_and_keep_fields() {
        let text = render_events(|| {
            tracing::warn!(address = "192.0.2.1", error = %"rejected\n\x1b[2J", "relay failed");
            tracing::debug!(target: "waywarp::test", "details");
        });
        assert_eq!(
            text,
            concat!(
                "warning: relay failed\n",
                "  address:   192.0.2.1\n",
                "  error:     rejected\\n\\u{1b}[2J\n",
                "debug: waywarp::test: details\n",
            )
        );
    }

    #[test]
    fn diagnostic_fields_align_to_the_longest_label() {
        let text = render_events(|| {
            tracing::warn!(
                destination = "192.0.2.1:443",
                error = "refused",
                "TCP connection failed"
            );
        });
        assert_eq!(
            text,
            concat!(
                "warning: TCP connection failed\n",
                "  destination: 192.0.2.1:443\n",
                "  error:       refused\n",
            )
        );
    }

    #[test]
    fn lifecycle_progress_is_lowercase_and_has_no_console_instance_prefix() {
        use crate::protocol::Progress;

        let name = "tokyo".parse().unwrap();
        for (progress, message) in [
            (Progress::Registering, "registering WARP"),
            (Progress::StartingDaemon, "starting warp-svc"),
            (Progress::Migrating, "moving tunnel to direct path"),
            (Progress::CheckingLocations, "checking geo and probe"),
            (
                Progress::Connecting {
                    route: "directly".into(),
                },
                "connecting directly",
            ),
            (
                Progress::Connecting {
                    route: "via tokyo".into(),
                },
                "connecting via tokyo",
            ),
        ] {
            assert_eq!(
                render_events(|| crate::output::progress(2, Some(&name), &progress)),
                format!("info: {message}\n"),
            );
        }
    }

    #[test]
    fn lifecycle_defaults_are_visible_without_enabling_unrelated_info() {
        for (directive, expected) in [(None, "info: stopped\n"), (Some("error"), "")] {
            assert_eq!(
                render_events_with_filter(LogMode::Command, directive, || {
                    tracing::info!(target: "waywarp::test", "unrelated diagnostic");
                    crate::output::stopped(2);
                }),
                expected,
            );
        }
    }

    #[test]
    fn file_logs_keep_structured_instance_context_and_plain_messages() {
        use crate::protocol::Progress;

        let name = "tokyo".parse().unwrap();
        let text = render_events_with_filter(LogMode::Detached, None, || {
            crate::output::progress(
                2,
                Some(&name),
                &Progress::Connecting {
                    route: "directly".into(),
                },
            );
        });
        assert!(text.contains("INFO"), "{text}");
        assert!(
            text.contains("waywarp::lifecycle: connecting directly"),
            "{text}"
        );
        assert!(text.contains("instance=2"), "{text}");
        assert!(text.contains("instance_name=\"tokyo\""), "{text}");
        assert!(!text.contains("2 tokyo:"), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(text.ends_with('\n'), "{text}");
    }

    #[test]
    fn lifecycle_failures_escape_routes_and_preserve_error_details() {
        use crate::protocol::{Failure, Progress};

        let text = render_events(|| {
            crate::output::progress(
                2,
                None,
                &Progress::AttemptFailed {
                    route: "via relay\nerror: fake\x1b[2J".into(),
                    failure: Failure {
                        message: "first line\nsecond line".into(),
                        causes: vec!["connection refused".into()],
                    },
                },
            );
        });
        assert_eq!(
            text,
            concat!(
                "warning: failed to connect via relay\\nerror: fake\\u{1b}[2J\n",
                "  error:     first line\\nsecond line\n",
                "  causes:    [\"connection refused\"]\n",
            )
        );
    }

    #[test]
    fn imports_use_the_same_tracing_formatter_and_escape_paths() {
        assert_eq!(
            render_events(|| {
                crate::output::imported(2, std::path::Path::new("/tmp/registration\n\x1b[2J"));
            }),
            "info: imported registration\n  from:      /tmp/registration\\n\\u{1b}[2J\n",
        );
    }
}
