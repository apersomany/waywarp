import json
import shlex
from typing import Any

# Resolve the service's current daemon on every command, including after restarts.
TOKYO_NAMESPACE = "nsenter -t $(pgrep -o -x warp-svc --cgroup /system.slice/waywarp-tokyo.service) -n "
CLI_NAMESPACE = "nsenter -t $(pgrep -n -x warp-svc) -n "
REGISTRATION_CONFIG = "/var/lib/waywarp/2/registration/conf.json"


def write_config(value: dict[str, Any], atomic: bool = False) -> None:
    # The watcher must follow both in-place edits and atomic replacement without a reconnect.
    target = REGISTRATION_CONFIG + ".new" if atomic else REGISTRATION_CONFIG
    machine.succeed("printf '%s' " + shlex.quote(json.dumps(value)) + " > " + target)
    if atomic:
        machine.succeed("mv " + target + " " + REGISTRATION_CONFIG)


def nat_rule(text: str, namespace: str = TOKYO_NAMESPACE) -> None:
    machine.wait_until_succeeds(
        namespace
        + "nft list chain inet waywarp postrouting | grep -F "
        + shlex.quote(text),
        timeout=30,
    )


def assert_snat_mapping(source: str, translated_source: str) -> None:
    # The stub's dummy link cannot reply, but conntrack records the exact outgoing SNAT.
    ipv6 = ":" in source
    destination = "2001:db8:ffff::1" if ipv6 else "198.51.100.1"
    ping_family = "-6 " if ipv6 else ""
    conntrack_family = "-f ipv6 " if ipv6 else ""
    machine.succeed(TOKYO_NAMESPACE + "conntrack -F")
    machine.execute(f"ping {ping_family}-c 1 -W 1 -I {source} {destination}")
    machine.succeed(
        TOKYO_NAMESPACE
        + f"conntrack -L {conntrack_family}2>/dev/null"
        + f" | grep -F 'src={source} dst={destination}'"
        + f" | grep -F 'src={destination} dst={translated_source} '"
    )


def physical_ipv4() -> str:
    return json.loads(machine.succeed("ip -j -4 route get 198.51.100.7"))[0]["prefsrc"]


def check_services_and_proxy() -> None:
    machine.wait_for_unit("waywarp-home.service")
    machine.wait_for_unit("waywarp-tokyo.service")

    with subtest("services do not accept terms by default or retry missing consent"):
        machine.wait_until_succeeds(
            "systemctl show waywarp-unaccepted.service -p ExecMainStatus --value | grep -Fx 2"
        )
        machine.succeed(
            "systemctl show waywarp-unaccepted.service -p NRestarts --value | grep -Fx 0"
        )
        machine.succeed("test ! -e /var/lib/waywarp/4")

    with subtest("new registrations require explicit consent before setup"):
        for mode in ["proxy", "bridge"]:
            code, output = machine.execute(
                "waywarp up " + mode + " 6 --name declined 2>&1"
            )
            assert code == 2, output
            assert "--accept-tos" in output, output
            assert "https://www.cloudflare.com/application/terms/" in output, output
            assert output.endswith(
                "Then pass --accept-tos to agree, or import an existing registration.\n"
            ), output
        machine.succeed("test ! -e /var/lib/waywarp/6")
        machine.fail("ip link show waywarp6")
        machine.succeed("waywarp up proxy 6 --accept-tos")
        machine.succeed(
            "grep -F 'waywarp::lifecycle: connecting directly instance=6' "
            "/var/lib/waywarp/6/waywarp.log"
        )
        machine.fail("grep -F '6: connecting directly' /var/lib/waywarp/6/waywarp.log")
        text = machine.succeed("waywarp down 6 2>&1")
        assert text == "info: stopped\n", text
        machine.succeed("waywarp up proxy 6")
        machine.fail("waywarp warp-cli 6 registration new")
        machine.succeed("waywarp warp-cli 6 --accept-tos registration new")
        machine.succeed("waywarp down 6")
        text = machine.succeed("waywarp import 7 --from /var/lib/waywarp/6/registration 2>&1")
        assert text == (
            "info: imported registration\n"
            "  from:      /var/lib/waywarp/6/registration\n"
        ), text
        machine.succeed("waywarp up proxy 7")
        machine.succeed("waywarp down 7")

    with subtest("status finds instances by index and name"):
        print(machine.succeed("waywarp status"))
        machine.succeed('waywarp status tokyo --json | grep -F \'"edge":"TST"\'')
        text = machine.succeed("waywarp status 0")
        assert text.startswith("Instance   0 (home)\nStatus     Connected\n"), text
        assert "Access     127.0.0.1:1080 (proxy)\n" in text, text
        assert "Edge       TST\n" in text, text
        fields = ["Bootstrap ", "Geo4 ", "Geo6 ", "Probe4 ", "Probe6 "]
        positions = [text.index(field) for field in fields]
        assert positions == sorted(positions), text
        assert all(not line.startswith(" ") for line in text.splitlines()), text
        assert "\n\n" not in text, text
        assert "Exit" not in text, text
        assert "\x1b" not in text, text
        machine.fail("waywarp status nobody")

    with subtest("foreground service readiness is printed only once"):
        journal = machine.succeed(
            "journalctl -u waywarp-home.service -o cat --no-pager"
        )
        assert journal.count("Instance   0 (home)\n") == 1, journal
        assert "info: starting warp-svc\n" in journal, journal
        assert "info: connecting directly\n" in journal, journal
        assert "info: checking geo and probe\n" in journal, journal
        assert "info: 0 home:" not in journal, journal
        assert "step:" not in journal, journal

    with subtest("proxy connections reach warp-svc inside the namespace"):
        machine.succeed("socat -T 5 - TCP:127.0.0.1:1080 </dev/null | grep -Fx stub")


def check_bridge_reconciliation() -> None:
    with subtest("bridge links the host to the namespace"):
        machine.succeed("ip -br address show waywarp2 | grep -F 10.9.0.6/30")
        machine.wait_until_succeeds("ping -c 1 -W 2 10.9.0.5", timeout=30)
        machine.wait_until_succeeds("ping -c 1 -W 2 fd77:6179:7761:7270::9", timeout=30)

    with subtest("bridge routes into WARP survive reconnects"):
        machine.succeed(
            TOKYO_NAMESPACE
            + "ip -4 route show table 79 | grep -F 'default dev CloudflareWARP'"
        )
        machine.succeed("waywarp warp-cli tokyo disconnect")
        # Let the stub's polling status listener observe the disconnect before reconnecting.
        machine.wait_until_succeeds(
            "(waywarp status tokyo || true) | grep -Fx 'Status     Disconnected (location mismatched)'", timeout=30
        )
        machine.succeed("waywarp warp-cli tokyo connect")
        # The stub recreated the link, so only a reapplied route can be present.
        for family in ["-4", "-6"]:
            machine.wait_until_succeeds(
                TOKYO_NAMESPACE
                + f"ip {family} route show table 79 | grep -F 'default dev CloudflareWARP'",
                timeout=30,
            )
        machine.succeed(
            TOKYO_NAMESPACE
            + "ip -4 route show table 80 | grep -F 'default via 10.9.0.6 dev veth'"
        )
        machine.succeed(
            TOKYO_NAMESPACE + "ip -4 rule show pref 101" + " | wc -l | grep -Fx 1"
        )

    with subtest("kernel link changes repair the bridge without a CLI reconnect"):
        machine.succeed(TOKYO_NAMESPACE + "ip link delete CloudflareWARP")
        # Link absence past the fallback interval must degrade health, not kill the service.
        machine.sleep(6)
        machine.succeed("systemctl is-active waywarp-tokyo.service")
        machine.succeed("(waywarp status tokyo || true) | grep -Fx 'Status     Degraded (location mismatched)'")
        machine.succeed(
            "(waywarp status tokyo --json || true) | grep -F '\"matched\":false'"
        )
        machine.succeed(
            TOKYO_NAMESPACE
            + "ip link add CloudflareWARP mtu 1360 type dummy; "
            + TOKYO_NAMESPACE
            + "ip link set CloudflareWARP up"
        )
        machine.succeed(
            TOKYO_NAMESPACE
            + "ip address add 172.16.0.2/32 dev CloudflareWARP; "
            + TOKYO_NAMESPACE
            + "ip -6 address add 2001:db8::2/128 dev CloudflareWARP nodad"
        )
        for family in ["-4", "-6"]:
            machine.wait_until_succeeds(
                TOKYO_NAMESPACE
                + f"ip {family} route show table 79 | grep -F 'default dev CloudflareWARP'",
                timeout=3,
            )
        machine.wait_until_succeeds(
            "ip -o link show waywarp2 | grep -F 'mtu 1360'", timeout=3
        )
        machine.succeed(TOKYO_NAMESPACE + "ip link set CloudflareWARP mtu 1280")
        machine.wait_until_succeeds(
            "ip -o link show waywarp2 | grep -F 'mtu 1280'", timeout=3
        )
        machine.wait_until_succeeds(
            'waywarp status tokyo --json | grep -F \'"state":"connected"\'', timeout=45
        )

    with subtest("bridge sizes the link to WARP and clamps TCP"):
        machine.succeed("ip -o link show waywarp2 | grep -F 'mtu 1280'")
        machine.succeed(TOKYO_NAMESPACE + "ip -o link show veth | grep -F 'mtu 1280'")
        machine.succeed(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp clamp | grep -F 'maxseg size set rt mtu'"
        )
        machine.succeed("ping -c 1 -W 2 -M do -s 1252 10.9.0.5")
        machine.fail("ping -c 1 -W 2 -M do -s 1253 10.9.0.5")

    with subtest("sockets bound to the bridge link reach WARP"):
        # The stub's link has no peer, so check the lookup rather than a reply.
        machine.succeed(
            "ip -4 rule show pref 32000 | grep -F 'oif waywarp2 lookup 2002876162'"
        )
        machine.succeed(
            "ip -6 rule show pref 32000 | grep -F 'oif waywarp2 lookup 2002876162'"
        )
        machine.succeed(
            "ip -4 route get 192.0.2.1 oif waywarp2 | grep -F 'via 10.9.0.5 dev waywarp2'"
        )
        machine.fail("ip -4 route get 192.0.2.1 | grep -F waywarp2")


def check_nat_policy() -> dict[str, Any]:
    policy: dict[str, Any] = {
        "account": {"account_type": "free"},
        "endpoints": [],
        "interface": {"v4": "172.16.0.2", "v6": "2001:db8::2"},
    }

    with subtest("consumer bridges SNAT only to assigned addresses"):
        nat_rule("snat ip to 172.16.0.2")
        nat_rule("snat ip6 to 2001:db8::2")
        machine.fail(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -F 'snat ip6 to 2001:db8::1'"
        )
        machine.succeed("ip address add 10.42.0.1/32 dev lo")
        machine.succeed("ip route add 198.51.100.1 via 10.9.0.5 dev waywarp2")
        assert_snat_mapping("10.42.0.1", "172.16.0.2")
        machine.succeed("ip -6 address add fd42::1/128 dev lo nodad")
        machine.succeed(
            "ip -6 route add 2001:db8:ffff::1 via fd77:6179:7761:7270::9 dev waywarp2"
        )
        assert_snat_mapping("fd42::1", "2001:db8::2")

    with subtest("connector routes follow live edits and NAT mode"):
        policy["connector_config"] = {
            "nat_mode": False,
            "routes": ["10.42.0.0/16", "fd42::/64"],
        }
        write_config(policy, atomic=True)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        nat_rule("ip6 saddr fd42::/64 accept")
        assert_snat_mapping("10.42.0.1", "10.42.0.1")
        assert_snat_mapping("fd42::1", "fd42::1")
        assert_snat_mapping("10.9.0.6", "172.16.0.2")
        machine.succeed(
            "(waywarp status tokyo --json || true) | grep -F '10.42.0.0/16'"
        )
        policy["connector_config"]["routes"] = []
        write_config(policy)
        machine.wait_until_fails(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'",
            timeout=30,
        )
        assert_snat_mapping("10.42.0.1", "172.16.0.2")
        assert_snat_mapping("fd42::1", "2001:db8::2")
        policy["connector_config"] = {"nat_mode": False, "routes": ["10.42.0.0/16"]}
        write_config(policy)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        policy["connector_config"]["nat_mode"] = True
        write_config(policy)
        machine.wait_until_fails(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'",
            timeout=30,
        )
        assert_snat_mapping("10.42.0.1", "172.16.0.2")

    with subtest("invalid config removes exemptions without choosing another address"):
        policy["connector_config"]["nat_mode"] = False
        write_config(policy)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        machine.succeed("printf '{' > " + REGISTRATION_CONFIG)
        machine.wait_until_succeeds(
            "(waywarp status tokyo --json || true) | grep -F '\"configuration_valid\":false'",
            timeout=30,
        )
        machine.fail(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'"
        )
        nat_rule("snat ip to 172.16.0.2")
        assert_snat_mapping("10.42.0.1", "172.16.0.2")
        write_config(policy, atomic=True)
        nat_rule("ip saddr 10.42.0.0/16 accept")

    with subtest("SNAT targets follow assigned addresses after reconnect"):
        machine.succeed("waywarp warp-cli tokyo disconnect")
        machine.wait_until_succeeds(
            "(waywarp status tokyo || true) | grep -Fx 'Status     Disconnected (location mismatched)'", timeout=30
        )
        policy["interface"] = {"v4": "100.96.0.37", "v6": "2001:db8::37"}
        write_config(policy, atomic=True)
        # New targets are not assigned yet: never fall back to the shared address.
        nat_rule("meta nfproto ipv4 drop")
        nat_rule("meta nfproto ipv6 drop")
        machine.fail(
            TOKYO_NAMESPACE + "nft list chain inet waywarp prerouting | grep -F dnat"
        )
        machine.succeed("waywarp warp-cli tokyo connect")
        nat_rule("snat ip to 100.96.0.37")
        nat_rule("snat ip6 to 2001:db8::37")
        machine.succeed(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp prerouting | grep -F 'ip daddr 100.96.0.37 dnat'"
        )
        machine.succeed(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp prerouting | grep -F 'ip6 daddr 2001:db8::37 dnat'"
        )
        machine.fail(
            TOKYO_NAMESPACE
            + "nft list chain inet waywarp prerouting | grep -E '172.16.0.2|2001:db8::2 '"
        )
        machine.wait_until_succeeds(
            TOKYO_NAMESPACE
            + "ip -4 route show table 79 | grep -F 'default dev CloudflareWARP'",
            timeout=30,
        )
        assert_snat_mapping("10.9.0.6", "100.96.0.37")
        assert_snat_mapping("fd42::1", "2001:db8::37")

    return policy


def check_connector_forwarding(policy: dict[str, Any]) -> None:
    with subtest("Team registrations enable native TCP forwarding"):
        policy["account"]["account_type"] = "team"
        policy["endpoints"] = [{"v4": "192.0.2.1:443"}]
        write_config(policy)
        # The client reads the registration at startup, not on a config watcher event.
        machine.succeed("systemctl restart waywarp-tokyo.service")
        machine.wait_for_unit("waywarp-tokyo.service")
        machine.wait_until_succeeds(
            "ping -6 -c 1 -W 2 fd77:6179:7761:7270::9", timeout=30
        )
        machine.succeed(
            TOKYO_NAMESPACE
            + "nft list chain ip waywarp_redirect output | grep -F 'redirect to'"
        )

    with subtest(
        "shared connector services stay local but device addresses reach the host"
    ):
        # Real ingress must enter on CloudflareWARP rather than as local OUTPUT traffic.
        machine.succeed(TOKYO_NAMESPACE + "ip link delete CloudflareWARP")
        machine.succeed(
            "ip link add warp-edge type veth peer name CloudflareWARP netns $(pgrep -o -x warp-svc --cgroup /system.slice/waywarp-tokyo.service)"
        )
        machine.succeed("ip netns add edge-test; ip link set warp-edge netns edge-test")
        edge = "ip netns exec edge-test "
        machine.succeed(
            edge
            + "ip link set warp-edge up; "
            + edge
            + "ip address add 198.18.0.2/32 dev warp-edge; "
            + edge
            + "ip -6 address add 2001:db8:eeee::2/128 dev warp-edge nodad"
        )
        machine.succeed(
            "ip route add 198.18.0.2 via 10.9.0.5 dev waywarp2; ip -6 route add 2001:db8:eeee::2 via fd77:6179:7761:7270::9 dev waywarp2"
        )
        machine.succeed(TOKYO_NAMESPACE + "ip link set CloudflareWARP mtu 1280 up")
        for address in [
            "100.96.0.37/32",
            "192.0.2.53/32",
            "2001:db8::37/128",
            "2001:db8::1/128",
        ]:
            machine.succeed(
                TOKYO_NAMESPACE
                + "ip address add "
                + address
                + " dev CloudflareWARP"
                + (" nodad" if ":" in address else "")
            )
        machine.succeed(
            TOKYO_NAMESPACE
            + "ip route add 198.18.0.2/32 dev CloudflareWARP; "
            + TOKYO_NAMESPACE
            + "ip -6 route add 2001:db8:eeee::2/128 dev CloudflareWARP"
        )
        for family in ["-4", "-6"]:
            machine.succeed(
                TOKYO_NAMESPACE
                + "ip "
                + family
                + " route replace default dev CloudflareWARP table 79"
            )
        for destination in ["100.96.0.37", "192.0.2.53"]:
            machine.succeed(edge + "ip route add " + destination + " dev warp-edge")
        machine.succeed(edge + "ip -6 route add 2001:db8::/64 dev warp-edge")
        nat_rule("snat ip6 to 2001:db8::37")
        # Distinct replies expose accidental host redirection for both TCP and UDP.
        physical_ip = physical_ipv4()
        listeners = [
            (edge, "TCP4-LISTEN:1053,bind=198.18.0.2,fork,reuseaddr", "tunnel"),
            ("", "TCP4-LISTEN:1053,bind=" + physical_ip + ",fork,reuseaddr", "uplink"),
            (
                TOKYO_NAMESPACE,
                "TCP4-LISTEN:1053,bind=192.0.2.53,fork,reuseaddr",
                "connector",
            ),
            (
                TOKYO_NAMESPACE,
                "TCP6-LISTEN:1053,bind=[2001:db8::1],fork,reuseaddr",
                "connector",
            ),
            (
                TOKYO_NAMESPACE,
                "UDP6-RECVFROM:1053,bind=[2001:db8::1],fork,reuseaddr",
                "connector",
            ),
            ("", "TCP4-LISTEN:1053,bind=10.9.0.6,fork,reuseaddr", "host"),
            (
                "",
                "TCP6-LISTEN:1053,bind=[fd77:6179:7761:7270::a],fork,reuseaddr",
                "host",
            ),
            (
                "",
                "UDP6-RECVFROM:1053,bind=[fd77:6179:7761:7270::a],fork,reuseaddr",
                "host",
            ),
        ]
        for index, (namespace, socket, reply) in enumerate(listeners):
            machine.succeed(
                namespace
                + "socat "
                + socket
                + " SYSTEM:'echo "
                + reply
                + "' >/tmp/connector-test-"
                + str(index)
                + ".log 2>&1 & echo $! >/tmp/connector-test-"
                + str(index)
                + ".pid"
            )
        for destination, reply in [
            ("192.0.2.53", "connector"),
            ("100.96.0.37", "host"),
        ]:
            machine.wait_until_succeeds(
                edge
                + "socat -T 2 - TCP4:"
                + destination
                + ":1053,connect-timeout=2 </dev/null | grep -Fx "
                + reply,
                timeout=30,
            )
        for destination, reply in [
            ("2001:db8::1", "connector"),
            ("2001:db8::37", "host"),
        ]:
            machine.wait_until_succeeds(
                edge
                + "socat -T 2 - TCP6:["
                + destination
                + "]:1053,connect-timeout=2 </dev/null | grep -Fx "
                + reply,
                timeout=30,
            )
            machine.wait_until_succeeds(
                "printf 'query\\n' | "
                + edge
                + "socat -T 2 - UDP6:["
                + destination
                + "]:1053 | grep -Fx "
                + reply,
                timeout=30,
            )
        with subtest(
            "native TCP preserves WARP routes and forwards physical-uplink traffic"
        ):
            # A broad redirect would steal WARP-routed connector control traffic.
            machine.wait_until_succeeds(
                TOKYO_NAMESPACE
                + "socat -T 2 - TCP4:198.18.0.2:1053,connect-timeout=2 </dev/null | grep -Fx tunnel",
                timeout=30,
            )
            # Untunneled TCP still needs the userspace uplink, not the UDP-only data plane.
            machine.wait_until_succeeds(
                TOKYO_NAMESPACE
                + "socat -T 2 - TCP4:"
                + physical_ip
                + ":1053,connect-timeout=2 </dev/null | grep -Fx uplink",
                timeout=30,
            )
        for index in range(len(listeners)):
            machine.succeed(
                "read pid </tmp/connector-test-" + str(index) + ".pid; kill $pid"
            )
        machine.succeed(edge + "ip link delete warp-edge")
        machine.succeed("ip netns delete edge-test")


def check_nat_overrides() -> None:
    with subtest("CLI NAT overrides work"):
        machine.succeed("systemctl stop waywarp-tokyo.service")
        machine.succeed(
            "waywarp up bridge tokyo --nat always --subnet4 10.9.0.4/30 --location edge=tst"
        )
        nat_rule("snat ip to 100.96.0.37", namespace=CLI_NAMESPACE)
        machine.fail(
            CLI_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'"
        )
        machine.succeed("waywarp down tokyo")
        machine.succeed(
            "waywarp up bridge tokyo --nat never --subnet4 10.9.0.4/30 --location edge=tst"
        )
        machine.fail(
            CLI_NAMESPACE
            + "nft list chain inet waywarp postrouting | grep -E 'snat|saddr|drop'"
        )
        machine.succeed(
            CLI_NAMESPACE
            + "nft list chain inet waywarp prerouting | grep -F 'ip6 daddr 2001:db8::37 dnat'"
        )
        machine.succeed("waywarp down tokyo")
        machine.succeed("systemctl start waywarp-tokyo.service")


def check_isolation_and_cleanup() -> None:
    with subtest("warp-svc resolves names without the host's nscd"):
        # Host nscd would bypass warp-svc's own DNS proxy and network namespace.
        machine.succeed("test -S /run/nscd/socket")
        machine.succeed(
            TOKYO_NAMESPACE.replace(" -n ", " -n -m ") + "test ! -e /run/nscd/socket"
        )

    with subtest("a bridge that fails to bootstrap removes what it created"):
        # Probes cannot reach Cloudflare here; failure occurs after link/firewall setup.
        machine.fail(
            "waywarp up bridge 5 --accept-tos --subnet4 10.9.0.20/30 --location probe4=tst"
        )
        machine.fail("ip link show waywarp5")
        machine.succeed("! ip -4 rule show pref 32000 | grep -F waywarp5")
        machine.succeed("! ip -6 rule show pref 32000 | grep -F waywarp5")

    with subtest("cancelling startup cleans up the bridge before exiting"):
        physical_ip = physical_ipv4()
        sleep_command = machine.succeed("command -v sleep").strip()
        machine.succeed(
            "systemd-run --unit=cancel-relay --property=Type=exec $(command -v socat) TCP4-LISTEN:2008,bind="
            + physical_ip
            + ",fork,reuseaddr SYSTEM:'"
            + sleep_command
            + " 60'"
        )
        machine.succeed(
            "systemd-run --unit=waywarp-cancel --property=Type=exec $(command -v waywarp) up bridge 8 --accept-tos --foreground --subnet4 10.9.0.32/30 --via socks5://"
            + physical_ip
            + ":2008"
        )
        machine.wait_until_succeeds(
            "ip link show waywarp8 || (journalctl -u waywarp-cancel.service -n 20 --no-pager; false)",
            timeout=60,
        )
        machine.succeed(
            "systemctl kill --kill-whom=main --signal=TERM waywarp-cancel.service"
        )
        machine.wait_until_fails(
            "systemctl is-active waywarp-cancel.service", timeout=30
        )
        machine.succeed(
            "systemctl show waywarp-cancel.service -p ExecMainStatus --value | grep -Fx 0"
        )
        machine.fail("ip link show waywarp8")
        machine.succeed("! ip -4 rule show pref 32000 | grep -F waywarp8")
        machine.succeed("! ip -6 rule show pref 32000 | grep -F waywarp8")
        machine.succeed("test ! -S /run/waywarp/8/control")
        machine.succeed("systemctl stop cancel-relay.service")

    with subtest("instances are locked while running"):
        machine.fail("waywarp up proxy home --listen 127.0.0.1:2000")
        machine.fail("waywarp import tokyo")

    with subtest("stopping an instance removes what it created"):
        machine.succeed("systemctl stop waywarp-tokyo.service")
        machine.fail("ip link show waywarp2")
        machine.succeed(
            'test -z "$(ip -4 rule show pref 32000; ip -6 rule show pref 32000)"'
        )
        machine.succeed("waywarp down home")
        machine.wait_until_fails("systemctl is-active waywarp-home.service")
        machine.succeed("waywarp status | grep -Fx 'no running instances'")


# Each phase builds on the previous one; reuse one VM and keep teardown last.
check_services_and_proxy()
check_bridge_reconciliation()
policy = check_nat_policy()
check_connector_forwarding(policy)
check_nat_overrides()
check_isolation_and_cleanup()
