use super::support::{Fixture, status};
use serde_json::{Value, json};

#[test]
fn empty_human_status_is_explicit_but_empty_json_is_silent() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    for json in [false, true] {
        let mut command = fixture.command();
        command.arg("status");
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            if json {
                b"".as_slice()
            } else {
                b"no running instances\n"
            }
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn human_status_and_unhealthy_json_have_the_same_health_exit_code() {
    for json in [false, true] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let expected = status(false);
        let response = expected.clone();
        let server = fixture.serve(move |request, descriptors| {
            assert_eq!(request, json!("Status"));
            assert!(descriptors.is_empty());
            Some(json!({ "Status": response }))
        });
        let mut command = fixture.command();
        command.args(["status", "2"]);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains('\x1b'));
        if json {
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), expected);
            assert_eq!(text.lines().count(), 1);
        } else {
            assert_eq!(
                text,
                concat!(
                    "Instance   2 (tokyo)\n",
                    "Status     Connected (location mismatched)\n",
                    "Access     127.0.0.1:1082 (proxy)\n",
                    "Edge       NRT\n",
                    "Bootstrap  Direct\n",
                    "Geo4       Unavailable\n",
                    "Geo6       Unavailable\n",
                    "Probe4     Unavailable\n",
                    "Probe6     Unavailable\n",
                )
            );
        }
    }
}

#[test]
fn remote_errors_keep_causes_on_stderr_without_corrupting_json() {
    for selected in [true, false] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let server = fixture.serve(|_, _| Some(json!({ "Failed": { "message": "reading instance", "causes": ["peer vanished", "connection reset"] } })));
        let mut command = fixture.command();
        command.arg("status");
        if selected {
            command.arg("2");
        }
        let output = command.arg("--json").output().unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "error: reading instance\n  caused by: peer vanished\n  caused by: connection reset\n"
        );
    }
}

#[test]
fn list_status_reports_malformed_responses() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let server = fixture.serve(|_, _| Some(json!({ "Status": "invalid" })));
    let output = fixture
        .command()
        .args(["status", "--json"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid type")
    );
}

#[test]
fn list_status_omits_stopped_instances_but_selected_status_still_fails() {
    for closed_after_request in [false, true] {
        for selected in [false, true] {
            let Some(fixture) = Fixture::new() else {
                return;
            };
            let server = if closed_after_request {
                Some(fixture.serve(|request, descriptors| {
                    assert_eq!(request, json!("Status"));
                    assert!(descriptors.is_empty());
                    None
                }))
            } else {
                drop(fixture.listener());
                None
            };
            let mut command = fixture.command();
            command.arg("status");
            if selected {
                command.arg("2");
            }
            let output = command.arg("--json").output().unwrap();
            if let Some(server) = server {
                server.join().unwrap();
            }
            assert!(output.stdout.is_empty());
            if selected {
                assert_eq!(output.status.code(), Some(1));
                assert_eq!(
                    String::from_utf8(output.stderr).unwrap(),
                    if closed_after_request {
                        "error: peer process exited\n"
                    } else {
                        "error: instance 2 is not running (root-owned instances are only visible with sudo)\n  caused by: ECONNREFUSED: Connection refused\n"
                    }
                );
            } else {
                assert!(output.status.success());
                assert!(output.stderr.is_empty());
            }
        }
    }
}
