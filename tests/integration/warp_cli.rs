use super::support::Fixture;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;

#[test]
fn warp_cli_is_a_byte_for_byte_passthrough_with_its_original_exit_code() {
    for (stdout_bytes, stderr_bytes) in [
        (
            b"{\"native\":true}\n".as_slice(),
            b"native warning\n".as_slice(),
        ),
        (
            b"{\"native\":true}".as_slice(),
            b"native warning".as_slice(),
        ),
    ] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let server = fixture.serve(move |request, descriptors| {
            assert_eq!(request, json!({ "WarpCli": ["--json", "status"] }));
            let [stdout, stderr]: [OwnedFd; 2] = descriptors.try_into().unwrap();
            File::from(stdout).write_all(stdout_bytes).unwrap();
            File::from(stderr).write_all(stderr_bytes).unwrap();
            Some(json!({ "Exit": 7 }))
        });
        let output = fixture
            .command()
            .args(["warp-cli", "2", "--json", "status"])
            .output()
            .unwrap();
        server.join().unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, stdout_bytes);
        assert_eq!(output.stderr, stderr_bytes);
    }
}
