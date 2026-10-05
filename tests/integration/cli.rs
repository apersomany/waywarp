use super::support::{Fixture, command, status};
use serde_json::json;
use std::process::Stdio;

#[test]
fn clap_help_version_and_usage_keep_their_streams_and_exit_codes() {
    let help_arguments: &[&[&str]] = &[
        &["--help"],
        &["--version"],
        &["up", "--help"],
        &["up", "proxy", "--help"],
        &["up", "bridge", "--help"],
        &["status", "--help"],
        &["down", "--help"],
        &["import", "--help"],
        &["warp-cli", "--help"],
    ];
    for arguments in help_arguments {
        let output = command().args(*arguments).output().unwrap();
        assert!(output.status.success(), "{arguments:?}");
        assert!(output.stdout.ends_with(b"\n"), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
        if *arguments == ["--version"] {
            assert_eq!(
                output.stdout,
                format!("waywarp {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
            );
        }
    }
    let invalid_arguments: &[&[&str]] = &[
        &["--not-an-option"],
        &["status", "256"],
        &["up", "proxy", "--listen"],
        &["up", "proxy", "--listen", "0.0.0.0:1080"],
        &["down", "--json"],
        &["warp-cli", "2"],
    ];
    for arguments in invalid_arguments {
        let output = command().args(*arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert!(output.stderr.ends_with(b"\n"), "{arguments:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("error:"),
            "{arguments:?}"
        );
    }
}

#[test]
fn a_closed_stdout_pipe_does_not_panic_or_report_a_failed_command() {
    for json in [false, true] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let server = fixture.serve(|_, _| Some(json!({ "Status": status(true) })));
        let (reader, writer) = nix::unistd::pipe().unwrap();
        drop(reader);
        let mut command = fixture.command();
        command.args(["status", "2"]);
        if json {
            command.arg("--json");
        }
        let output = command.stdout(Stdio::from(writer)).output().unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn down_reports_through_tracing_on_stderr_and_respects_explicit_filters() {
    for (directive, expected) in [(None, "info: stopped\n"), (Some("error"), "")] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let server = fixture.serve(|request, descriptors| {
            assert_eq!(request, json!("Stop"));
            assert!(descriptors.is_empty());
            None
        });
        let mut command = fixture.command();
        command.args(["down", "2"]);
        if let Some(directive) = directive {
            command.env("WAYWARP_LOG", directive);
        }
        let output = command.output().unwrap();
        server.join().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
    }
}
