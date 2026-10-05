use super::support::Fixture;
use std::fs;

#[test]
fn missing_consent_errors_end_with_a_newline_before_setup() {
    for access in ["proxy", "bridge"] {
        for foreground in [false, true] {
            let Some(fixture) = Fixture::new() else {
                return;
            };
            let mut command = fixture.command();
            command.args(["up", access]);
            if foreground {
                command.arg("--foreground");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                concat!(
                    "error: a new WARP registration requires accepting Cloudflare's Terms of Service.\n\n",
                    "Review: https://www.cloudflare.com/application/terms/\n",
                    "Then pass --accept-tos to agree, or import an existing registration.\n",
                ),
                "{access}, foreground={foreground}",
            );
            assert!(!fixture.path().join("state/waywarp/0").exists());
            assert!(!fixture.path().join("runtime/waywarp/0/control").exists());
        }
    }
}

#[test]
fn failed_startup_preflight_preserves_the_existing_name() {
    for failure in ["interface", "configuration"] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let state = fixture.path().join("state/waywarp/2");
        let registration = state.join("registration");
        fs::create_dir_all(&registration).unwrap();
        fs::write(registration.join("reg.json"), "{}\n").unwrap();
        fs::write(state.join("name"), "old\n").unwrap();
        if failure == "configuration" {
            fs::write(registration.join("conf.json"), "not json\n").unwrap();
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = listener.local_addr().unwrap().to_string();
        drop(listener);
        let mut command = fixture.command();
        command.args(["up", "proxy", "2", "--name", "new", "--listen", &listen]);
        if failure == "interface" {
            command.args(["--interface", "waywarp-no-such-interface"]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains(if failure == "interface" {
                "interface waywarp-no-such-interface does not exist"
            } else {
                "registration/conf.json"
            }),
            "{error}"
        );
        assert_eq!(fs::read_to_string(state.join("name")).unwrap(), "old\n");
        assert!(!state.join("waywarp.log").exists());
        assert_eq!(
            fs::read_dir(fixture.path().join("runtime/waywarp/2"))
                .unwrap()
                .count(),
            1
        );
    }
}
