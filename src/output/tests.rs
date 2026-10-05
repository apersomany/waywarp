use super::*;
use crate::location::{Locations, probe::Probe};
use crate::warp::State;

fn proxy() -> Status {
    Status {
        index: 2,
        name: Some("tokyo".parse().unwrap()),
        access: Access::Proxy {
            listen: "127.0.0.1:1082".parse().unwrap(),
        },
        state: State::Connected,
        locations: Locations {
            edge: "NRT".into(),
            geo4: Some("JP/Tokyo".parse().unwrap()),
            probe4: Some(Probe {
                address: "203.0.113.4".parse().unwrap(),
                colo: "NRT".into(),
            }),
            ..Locations::default()
        },
        relay: None,
        matched: true,
        rebootstraps: 0,
        nat: None,
    }
}

fn render(render: impl FnOnce(&mut Renderer<Vec<u8>>) -> io::Result<()>) -> String {
    let mut renderer = Renderer {
        writer: Vec::new(),
        color: false,
    };
    render(&mut renderer).unwrap();
    String::from_utf8(renderer.writer).unwrap()
}

#[test]
fn proxy_status_groups_geo_and_probe_with_aligned_values() {
    let mut status = proxy();
    status.locations.geo6 = Some("JP/Tokyo".parse().unwrap());
    status.locations.probe6 = Some(Probe {
        address: "2001:db8::4".parse().unwrap(),
        colo: "NRT".into(),
    });
    assert_eq!(
        render(|renderer| renderer.status(&status)),
        concat!(
            "Instance   2 (tokyo)\n",
            "Status     Connected\n",
            "Access     127.0.0.1:1082 (proxy)\n",
            "Edge       NRT\n",
            "Bootstrap  Direct\n",
            "Geo4       JP/Tokyo\n",
            "Geo6       JP/Tokyo\n",
            "Probe4     203.0.113.4 (NRT)\n",
            "Probe6     2001:db8::4 (NRT)\n",
        )
    );
}

#[test]
fn unavailable_locations_keep_the_same_order_and_columns() {
    let mut status = proxy();
    status.locations = Locations::default();
    assert_eq!(
        render(|renderer| renderer.status(&status)),
        concat!(
            "Instance   2 (tokyo)\n",
            "Status     Connected\n",
            "Access     127.0.0.1:1082 (proxy)\n",
            "Edge       Unavailable\n",
            "Bootstrap  Direct\n",
            "Geo4       Unavailable\n",
            "Geo6       Unavailable\n",
            "Probe4     Unavailable\n",
            "Probe6     Unavailable\n",
        )
    );
}

#[test]
fn instance_keys_and_values_share_color_and_align_with_other_rows() {
    for named in [false, true] {
        for color in [false, true] {
            let mut status = proxy();
            if !named {
                status.name = None;
            }
            let mut renderer = Renderer {
                writer: Vec::new(),
                color,
            };
            renderer.status(&status).unwrap();
            let text = String::from_utf8(renderer.writer).unwrap();
            let value = if named { "2 (tokyo)" } else { "2" };
            let row = format!("Instance   {value}");
            assert_eq!(
                text.lines().next().unwrap(),
                if color {
                    format!("\x1b[36m{row}\x1b[0m")
                } else {
                    row.clone()
                },
            );
            assert_eq!(&row[11..], value);
            assert!(text.lines().skip(1).all(|line| !line.starts_with(' ')));
            assert_eq!(text.contains('\x1b'), color);
        }
    }
}

#[test]
fn status_states_preserve_their_meaning_and_health_color() {
    for (state, label) in [
        (State::Unknown, "Unknown"),
        (State::Disconnected, "Disconnected"),
        (State::Unable, "Unable to connect"),
        (State::Connecting, "Connecting"),
        (State::Degraded, "Degraded"),
        (State::Connected, "Connected"),
    ] {
        let mut status = proxy();
        status.state = state;
        let mut renderer = Renderer {
            writer: Vec::new(),
            color: true,
        };
        renderer.status(&status).unwrap();
        let text = String::from_utf8(renderer.writer).unwrap();
        let color = if status.healthy() { "32" } else { "33" };
        assert!(text.contains(&format!("Status     \x1b[{color}m{label}\x1b[0m\n")));
    }
}

#[test]
fn bridge_addresses_nat_and_continuations_share_the_value_column() {
    let mut status = proxy();
    status.access = Access::Bridge {
        link: "waywarp2".into(),
        subnets: crate::bridge::Subnets::new(
            2,
            Some("10.9.0.4/30".parse().unwrap()),
            Some("fd42::8/126".parse().unwrap()),
        ),
        nat: Mode::Auto,
    };
    status.nat = Some(crate::bridge::nat::Policy {
        mode: Mode::Auto,
        v4: Some("172.16.0.2".parse().unwrap()),
        v6: None,
        routed: vec![
            "10.42.0.0/16".parse().unwrap(),
            "fd42::/64".parse().unwrap(),
        ],
        configuration_valid: false,
    });
    let text = render(|renderer| renderer.status(&status));
    assert!(text.starts_with(concat!(
        "Instance   2 (tokyo)\n",
        "Status     Connected\n",
        "Access     waywarp2 (bridge)\n",
        "Link4      10.9.0.6/30 via 10.9.0.5\n",
        "Link6      fd42::a/126 via fd42::9\n",
        "Edge       NRT\n",
        "Bootstrap  Direct\n",
    )));
    assert!(text.ends_with(concat!(
        "NAT        Auto\n",
        "SNAT4      172.16.0.2\n",
        "SNAT6      blocked (no verified address)\n",
        "Routed     10.42.0.0/16\n",
        "           fd42::/64\n",
        "Warning    registration unreadable; routed exemptions removed\n",
        "           using last verified addresses\n",
    )));
}

#[test]
fn disabled_nat_omits_snat_and_keeps_retry_counts_compact() {
    let mut status = proxy();
    status.nat = Some(crate::bridge::nat::Policy {
        mode: Mode::Never,
        configuration_valid: true,
        ..Default::default()
    });
    for retries in [1, 2] {
        status.rebootstraps = retries;
        let text = render(|renderer| renderer.status(&status));
        assert!(!text.contains("SNAT"));
        assert!(text.ends_with(&format!(
            "NAT        Never\nRouted     None\nRetries    {retries} rebootstrap{}\n",
            if retries == 1 { "" } else { "s" },
        )));
    }
}

#[test]
fn multiple_statuses_have_one_blank_line_between_instances() {
    let status = proxy();
    let single = render(|renderer| renderer.status(&status));
    assert_eq!(
        render(|renderer| renderer.statuses(&[status.clone(), status], false)),
        format!("{single}\n{single}")
    );
}

#[test]
fn multiline_messages_align_under_the_first_line() {
    assert_eq!(
        render(|renderer| renderer.message(Kind::Warning, "first line\nsecond line")),
        "warning: first line\n         second line\n"
    );
}

#[test]
fn terminal_color_does_not_change_continuation_alignment() {
    let mut renderer = Renderer {
        writer: Vec::new(),
        color: true,
    };
    renderer
        .message(Kind::Warning, "first line\nsecond line")
        .unwrap();
    assert_eq!(
        String::from_utf8(renderer.writer).unwrap(),
        "\x1b[33mwarning\x1b[0m: first line\n         second line\n"
    );
}

#[test]
fn json_is_unchanged_ndjson_even_on_a_colored_terminal() {
    let mut status = proxy();
    status.relay = Some("untrusted\x1b[2J\nrelay".into());
    let mut renderer = Renderer {
        writer: Vec::new(),
        color: true,
    };
    renderer
        .statuses(&[status.clone(), status.clone()], true)
        .unwrap();
    let text = String::from_utf8(renderer.writer).unwrap();
    assert!(!text.contains('\x1b'));
    assert_eq!(text.lines().count(), 2);
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value, serde_json::to_value(&status).unwrap());
    }
}

#[test]
fn errors_preserve_causes_and_indent_multiline_helper_stderr() {
    let error = anyhow::anyhow!("first line\nsecond line\r\x1b[2J").context("running warp-cli");
    assert_eq!(
        render(|renderer| renderer.failure(Kind::Error, &Failure::from(&error))),
        concat!(
            "error: running warp-cli\n",
            "  caused by: first line\n",
            "             second line\\r\\u{1b}[2J\n",
        )
    );
}

#[test]
fn untrusted_status_fields_cannot_inject_lines_or_terminal_sequences() {
    let mut status = proxy();
    status.relay = Some("relay\nerror: fake\x1b[2J\t".into());
    status.locations.geo4.as_mut().unwrap().city = Some("Tokyo\x07".into());
    let text = render(|renderer| renderer.status(&status));
    assert!(text.contains("Bootstrap  relay\\nerror: fake\\u{1b}[2J\\t\n"));
    assert!(text.contains("JP/Tokyo\\u{7}"));
    assert!(!text.contains('\x1b'));
    assert!(!text.contains('\x07'));
}

#[test]
fn only_stdout_broken_pipes_are_successful_command_termination() {
    let stdout_error = anyhow::Error::new(StdoutError(io::Error::from(io::ErrorKind::BrokenPipe)))
        .context("showing status");
    assert!(is_broken_pipe(&stdout_error));
    let network_error = anyhow::Error::new(io::Error::from(io::ErrorKind::BrokenPipe))
        .context("contacting supervisor");
    assert!(!is_broken_pipe(&network_error));
    assert!(!is_broken_pipe(&anyhow::Error::new(StdoutError(
        io::Error::from(io::ErrorKind::PermissionDenied)
    ))));
}
