{ self, warp-stub }:
{
  name = "waywarp";
  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];
      environment.systemPackages = [
        pkgs.nftables
        pkgs.socat
        pkgs.conntrack-tools
      ];
      boot.kernelModules = [ "dummy" ];
      networking.firewall.allowedTCPPorts = [ 1053 ];
      networking.firewall.allowedUDPPorts = [ 1053 ];
      services.waywarp = {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default.override {
          cloudflare-warp = warp-stub;
        };
        instances = {
          home = {
            index = 0;
            acceptTos = true;
            access.proxy = { };
          };
          tokyo = {
            index = 2;
            acceptTos = true;
            access.bridge.subnet4 = "10.9.0.4/30";
            location = "edge=tst";
          };
          unaccepted = {
            index = 4;
            access.proxy = { };
          };
        };
      };
    };

  testScript = ''
    machine.wait_for_unit("waywarp-home.service")
    machine.wait_for_unit("waywarp-tokyo.service")

    with subtest("services do not accept terms by default or retry missing consent"):
        machine.wait_until_succeeds("systemctl show waywarp-unaccepted.service -p ExecMainStatus --value | grep -Fx 2")
        machine.succeed("systemctl show waywarp-unaccepted.service -p NRestarts --value | grep -Fx 0")
        machine.succeed("test ! -e /var/lib/waywarp/4")

    with subtest("new registrations require explicit consent before setup"):
        for mode in ["proxy", "bridge"]:
            code, output = machine.execute("waywarp up " + mode + " 6 --name declined 2>&1")
            assert code == 2, output
            assert "--accept-tos" in output, output
            assert "https://www.cloudflare.com/application/terms/" in output, output
        machine.succeed("test ! -e /var/lib/waywarp/6")
        machine.fail("ip link show waywarp6")
        machine.succeed("waywarp up proxy 6 --accept-tos")
        machine.succeed("waywarp down 6")
        machine.succeed("waywarp up proxy 6")
        machine.fail("waywarp warp-cli 6 registration new")
        machine.succeed("waywarp warp-cli 6 --accept-tos registration new")
        machine.succeed("waywarp down 6")
        machine.succeed("waywarp import 7 --from /var/lib/waywarp/6/registration")
        machine.succeed("waywarp up proxy 7")
        machine.succeed("waywarp down 7")

    with subtest("status finds instances by index and name"):
        print(machine.succeed("waywarp status"))
        machine.succeed("waywarp status tokyo --json | grep -F '\"edge\":\"TST\"'")
        machine.succeed("waywarp status 0 | grep -F '0 home: connected'")
        machine.fail("waywarp status nobody")

    with subtest("proxy connections reach warp-svc inside the namespace"):
        machine.succeed("socat -T 5 - TCP:127.0.0.1:1080 </dev/null | grep -Fx stub")

    with subtest("bridge links the host to the namespace"):
        machine.succeed("ip -br address show waywarp2 | grep -F 10.9.0.6/30")
        machine.wait_until_succeeds("ping -c 1 -W 2 10.9.0.5", timeout=30)
        machine.wait_until_succeeds("ping -c 1 -W 2 fd77:6179:7761:7270::9", timeout=30)

    # Runs a command inside the bridged instance's private network namespace, where the
    # instance's own warp-svc lives. `pgrep -f` also matches the pgrep shell, so match by unit.
    in_tokyo = "nsenter -t $(pgrep -o -x warp-svc --cgroup /system.slice/waywarp-tokyo.service) -n "

    with subtest("bridge routes into WARP survive reconnects"):
        machine.succeed(in_tokyo + "ip -4 route show table 79 | grep -F 'default dev CloudflareWARP'")
        machine.succeed("waywarp warp-cli tokyo disconnect")
        # The stub's status listener polls, so let it observe the disconnect before reconnecting.
        # `status` exits nonzero while disconnected, so read it without requiring success.
        machine.wait_until_succeeds("(waywarp status tokyo || true) | grep -F 'tokyo: disconnected'", timeout=30)
        machine.succeed("waywarp warp-cli tokyo connect")
        # The stub recreated the link, so only a reapplied route can be present.
        for family in ["-4", "-6"]:
            machine.wait_until_succeeds(
                in_tokyo + f"ip {family} route show table 79 | grep -F 'default dev CloudflareWARP'",
                timeout=30,
            )
        machine.succeed(in_tokyo + "ip -4 route show table 80 | grep -F 'default via 10.9.0.6 dev veth'")
        machine.succeed(in_tokyo + "ip -4 rule show pref 101" + " | wc -l | grep -Fx 1")

    with subtest("bridge sizes the link to WARP and clamps TCP"):
        machine.succeed("ip -o link show waywarp2 | grep -F 'mtu 1280'")
        machine.succeed(in_tokyo + "ip -o link show veth | grep -F 'mtu 1280'")
        machine.succeed(in_tokyo + "nft list chain inet waywarp clamp | grep -F 'maxseg size set rt mtu'")
        # The host refuses oversized packets itself, so senders learn the path MTU.
        machine.succeed("ping -c 1 -W 2 -M do -s 1252 10.9.0.5")
        machine.fail("ping -c 1 -W 2 -M do -s 1253 10.9.0.5")

    with subtest("sockets bound to the bridge link reach WARP"):
        # The stub's link has no peer, so check the lookup rather than a reply.
        machine.succeed("ip -4 rule show pref 32000 | grep -F 'oif waywarp2 lookup 2002876162'")
        machine.succeed("ip -6 rule show pref 32000 | grep -F 'oif waywarp2 lookup 2002876162'")
        machine.succeed("ip -4 route get 192.0.2.1 oif waywarp2 | grep -F 'via 10.9.0.5 dev waywarp2'")
        # Traffic not bound to the link keeps using the host's own routes.
        machine.fail("ip -4 route get 192.0.2.1 | grep -F waywarp2")

    # Registration writes include in-place edits and atomic replacement, as warp-svc may use
    # either. The watcher must follow both without a reconnect or a service restart.
    import json
    import shlex
    conf = "/var/lib/waywarp/2/registration/conf.json"
    policy = {"account": {"account_type": "free"}, "endpoints": [], "interface": {"v4": "172.16.0.2", "v6": "2001:db8::2"}}
    def write_config(value, atomic=False):
        target = conf + ".new" if atomic else conf
        machine.succeed("printf '%s' " + shlex.quote(json.dumps(value)) + " > " + target)
        if atomic:
            machine.succeed("mv " + target + " " + conf)

    def nat_rule(text):
        machine.wait_until_succeeds(in_tokyo + "nft list chain inet waywarp postrouting | grep -F " + shlex.quote(text), timeout=30)

    def flow(source, reply_destination):
        # The stub's dummy link cannot reply, but conntrack records the exact outgoing SNAT.
        machine.succeed(in_tokyo + "conntrack -F")
        machine.execute("ping -c 1 -W 1 -I " + source + " 198.51.100.1")
        machine.succeed(in_tokyo + "conntrack -L 2>/dev/null | grep -F 'src=" + source + " dst=198.51.100.1' | grep -F 'src=198.51.100.1 dst=" + reply_destination + " '")

    def flow6(source, reply_destination):
        machine.succeed(in_tokyo + "conntrack -F")
        machine.execute("ping -6 -c 1 -W 1 -I " + source + " 2001:db8:ffff::1")
        machine.succeed(in_tokyo + "conntrack -L -f ipv6 2>/dev/null | grep -F 'src=" + source + " dst=2001:db8:ffff::1' | grep -F 'src=2001:db8:ffff::1 dst=" + reply_destination + " '")

    with subtest("consumer bridges SNAT only to assigned addresses"):
        nat_rule("snat ip to 172.16.0.2")
        nat_rule("snat ip6 to 2001:db8::2")
        machine.fail(in_tokyo + "nft list chain inet waywarp postrouting | grep -F 'snat ip6 to 2001:db8::1'")
        machine.succeed("ip address add 10.42.0.1/32 dev lo")
        machine.succeed("ip route add 198.51.100.1 via 10.9.0.5 dev waywarp2")
        flow("10.42.0.1", "172.16.0.2")
        machine.succeed("ip -6 address add fd42::1/128 dev lo nodad")
        machine.succeed("ip -6 route add 2001:db8:ffff::1 via fd77:6179:7761:7270::9 dev waywarp2")
        flow6("fd42::1", "2001:db8::2")

    with subtest("connector routes follow live edits and NAT mode"):
        policy["connector_config"] = {"nat_mode": False, "routes": ["10.42.0.0/16", "fd42::/64"]}
        write_config(policy, atomic=True)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        nat_rule("ip6 saddr fd42::/64 accept")
        flow("10.42.0.1", "10.42.0.1")
        flow6("fd42::1", "fd42::1")
        flow("10.9.0.6", "172.16.0.2")
        machine.succeed("waywarp status tokyo --json | grep -F '10.42.0.0/16'")
        policy["connector_config"]["routes"] = []
        write_config(policy)
        machine.wait_until_fails(in_tokyo + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'", timeout=30)
        flow("10.42.0.1", "172.16.0.2")
        flow6("fd42::1", "2001:db8::2")
        policy["connector_config"] = {"nat_mode": False, "routes": ["10.42.0.0/16"]}
        write_config(policy)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        policy["connector_config"]["nat_mode"] = True
        write_config(policy)
        machine.wait_until_fails(in_tokyo + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'", timeout=30)
        flow("10.42.0.1", "172.16.0.2")

    with subtest("invalid config removes exemptions without choosing another address"):
        policy["connector_config"]["nat_mode"] = False
        write_config(policy)
        nat_rule("ip saddr 10.42.0.0/16 accept")
        machine.succeed("printf '{' > " + conf)
        machine.wait_until_succeeds("waywarp status tokyo --json | grep -F '\"configuration_valid\":false'", timeout=30)
        machine.fail(in_tokyo + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'")
        nat_rule("snat ip to 172.16.0.2")
        flow("10.42.0.1", "172.16.0.2")
        write_config(policy, atomic=True)
        nat_rule("ip saddr 10.42.0.0/16 accept")

    with subtest("SNAT targets follow assigned addresses after reconnect"):
        machine.succeed("waywarp warp-cli tokyo disconnect")
        machine.wait_until_succeeds("(waywarp status tokyo || true) | grep -F 'tokyo: disconnected'", timeout=30)
        policy["interface"] = {"v4": "100.96.0.37", "v6": "2001:db8::37"}
        write_config(policy, atomic=True)
        # New targets are not yet assigned: never fall back to the shared address.
        nat_rule("meta nfproto ipv4 drop")
        nat_rule("meta nfproto ipv6 drop")
        machine.fail(in_tokyo + "nft list chain inet waywarp prerouting | grep -F dnat")
        machine.succeed("waywarp warp-cli tokyo connect")
        nat_rule("snat ip to 100.96.0.37")
        nat_rule("snat ip6 to 2001:db8::37")
        machine.succeed(in_tokyo + "nft list chain inet waywarp prerouting | grep -F 'ip daddr 100.96.0.37 dnat'")
        machine.succeed(in_tokyo + "nft list chain inet waywarp prerouting | grep -F 'ip6 daddr 2001:db8::37 dnat'")
        machine.fail(in_tokyo + "nft list chain inet waywarp prerouting | grep -E '172.16.0.2|2001:db8::2 '")
        machine.wait_until_succeeds(in_tokyo + "ip -4 route show table 79 | grep -F 'default dev CloudflareWARP'", timeout=30)
        flow("10.9.0.6", "100.96.0.37")
        flow6("fd42::1", "2001:db8::37")

    with subtest("Team registrations enable native TCP forwarding"):
        policy["account"]["account_type"] = "team"
        policy["endpoints"] = [{"v4": "192.0.2.1:443"}]
        write_config(policy)
        # The client reads the registration when starting, not on a config watcher event.
        machine.succeed("systemctl restart waywarp-tokyo.service")
        machine.wait_for_unit("waywarp-tokyo.service")
        machine.wait_until_succeeds("ping -6 -c 1 -W 2 fd77:6179:7761:7270::9", timeout=30)
        machine.succeed(in_tokyo + "nft list chain ip waywarp_redirect output | grep -F 'redirect to'")

    with subtest("shared connector services stay local but device addresses reach the host"):
        # Replace the stub's dummy with a real ingress link. The host end simulates the edge;
        # packets therefore enter the namespace on CloudflareWARP, not as local OUTPUT traffic.
        machine.succeed(in_tokyo + "ip link delete CloudflareWARP")
        machine.succeed("ip link add warp-edge type veth peer name CloudflareWARP netns $(pgrep -o -x warp-svc --cgroup /system.slice/waywarp-tokyo.service)")
        machine.succeed("ip netns add edge-test; ip link set warp-edge netns edge-test")
        edge = "ip netns exec edge-test "
        machine.succeed(edge + "ip link set warp-edge up; " + edge + "ip address add 198.18.0.2/32 dev warp-edge; " + edge + "ip -6 address add 2001:db8:eeee::2/128 dev warp-edge nodad")
        machine.succeed("ip route add 198.18.0.2 via 10.9.0.5 dev waywarp2; ip -6 route add 2001:db8:eeee::2 via fd77:6179:7761:7270::9 dev waywarp2")
        machine.succeed(in_tokyo + "ip link set CloudflareWARP mtu 1280 up")
        for address in ["100.96.0.37/32", "192.0.2.53/32", "2001:db8::37/128", "2001:db8::1/128"]:
            machine.succeed(in_tokyo + "ip address add " + address + " dev CloudflareWARP" + (" nodad" if ":" in address else ""))
        machine.succeed(in_tokyo + "ip route add 198.18.0.2/32 dev CloudflareWARP; " + in_tokyo + "ip -6 route add 2001:db8:eeee::2/128 dev CloudflareWARP")
        for family in ["-4", "-6"]:
            machine.succeed(in_tokyo + "ip " + family + " route replace default dev CloudflareWARP table 79")
        for destination in ["100.96.0.37", "192.0.2.53"]:
            machine.succeed(edge + "ip route add " + destination + " dev warp-edge")
        machine.succeed(edge + "ip -6 route add 2001:db8::/64 dev warp-edge")
        nat_rule("snat ip6 to 2001:db8::37")
        # Distinct replies make accidental host redirection observable, for TCP and UDP.
        physical_ip = json.loads(machine.succeed("ip -j -4 route get 198.51.100.7"))[0]["prefsrc"]
        listeners = [
            (edge, "TCP4-LISTEN:1053,bind=198.18.0.2,fork,reuseaddr", "tunnel"),
            ("", "TCP4-LISTEN:1053,bind=" + physical_ip + ",fork,reuseaddr", "uplink"),
            (in_tokyo, "TCP4-LISTEN:1053,bind=192.0.2.53,fork,reuseaddr", "connector"),
            (in_tokyo, "TCP6-LISTEN:1053,bind=[2001:db8::1],fork,reuseaddr", "connector"),
            (in_tokyo, "UDP6-RECVFROM:1053,bind=[2001:db8::1],fork,reuseaddr", "connector"),
            ("", "TCP4-LISTEN:1053,bind=10.9.0.6,fork,reuseaddr", "host"),
            ("", "TCP6-LISTEN:1053,bind=[fd77:6179:7761:7270::a],fork,reuseaddr", "host"),
            ("", "UDP6-RECVFROM:1053,bind=[fd77:6179:7761:7270::a],fork,reuseaddr", "host"),
        ]
        for index, (namespace, socket, reply) in enumerate(listeners):
            machine.succeed(namespace + "socat " + socket + " SYSTEM:'echo " + reply + "' >/tmp/connector-test-" + str(index) + ".log 2>&1 & echo $! >/tmp/connector-test-" + str(index) + ".pid")
        for destination, reply in [("192.0.2.53", "connector"), ("100.96.0.37", "host")]:
            machine.wait_until_succeeds(edge + "socat -T 2 - TCP4:" + destination + ":1053,connect-timeout=2 </dev/null | grep -Fx " + reply, timeout=30)
        for destination, reply in [("2001:db8::1", "connector"), ("2001:db8::37", "host")]:
            machine.wait_until_succeeds(edge + "socat -T 2 - TCP6:[" + destination + "]:1053,connect-timeout=2 </dev/null | grep -Fx " + reply, timeout=30)
            machine.wait_until_succeeds("printf 'query\\n' | " + edge + "socat -T 2 - UDP6:[" + destination + "]:1053 | grep -Fx " + reply, timeout=30)
        with subtest("native TCP preserves WARP routes and forwards physical-uplink traffic"):
            # Model connector control traffic: local OUTPUT is routed through WARP, not tun.
            # A broad output redirect steals it and forwards it through the physical uplink.
            machine.wait_until_succeeds(in_tokyo + "socat -T 2 - TCP4:198.18.0.2:1053,connect-timeout=2 </dev/null | grep -Fx tunnel", timeout=30)
            # Untunneled TCP still needs the userspace uplink; otherwise the emulated TUN
            # carries TCP packets to a UDP-only data plane and the connection cannot complete.
            machine.wait_until_succeeds(in_tokyo + "socat -T 2 - TCP4:" + physical_ip + ":1053,connect-timeout=2 </dev/null | grep -Fx uplink", timeout=30)
        for index in range(len(listeners)):
            machine.succeed("read pid </tmp/connector-test-" + str(index) + ".pid; kill $pid")
        machine.succeed(edge + "ip link delete warp-edge")
        machine.succeed("ip netns delete edge-test")

    with subtest("CLI NAT overrides work"):
        machine.succeed("systemctl stop waywarp-tokyo.service")
        service_namespace = in_tokyo
        in_tokyo = "nsenter -t $(pgrep -n -x warp-svc) -n "
        machine.succeed("waywarp up bridge tokyo --nat always --subnet4 10.9.0.4/30 --location edge=tst")
        nat_rule("snat ip to 100.96.0.37")
        machine.fail(in_tokyo + "nft list chain inet waywarp postrouting | grep -F 'saddr 10.42.0.0/16'")
        machine.succeed("waywarp down tokyo")
        machine.succeed("waywarp up bridge tokyo --nat never --subnet4 10.9.0.4/30 --location edge=tst")
        machine.fail(in_tokyo + "nft list chain inet waywarp postrouting | grep -E 'snat|saddr|drop'")
        machine.succeed(in_tokyo + "nft list chain inet waywarp prerouting | grep -F 'ip6 daddr 2001:db8::37 dnat'")
        machine.succeed("waywarp down tokyo")
        machine.succeed("systemctl start waywarp-tokyo.service")
        in_tokyo = service_namespace

    with subtest("warp-svc resolves names without the host's nscd"):
        # nscd would answer from the host's network namespace instead of warp-svc's DNS proxy.
        machine.succeed("test -S /run/nscd/socket")
        machine.succeed(in_tokyo.replace(" -n ", " -n -m ") + "test ! -e /run/nscd/socket")

    with subtest("a bridge that fails to bootstrap removes what it created"):
        # Probes cannot reach Cloudflare in the VM, so probe4 never matches and bootstrap fails
        # after the link, rules, and firewall already exist.
        machine.fail("waywarp up bridge 5 --accept-tos --location probe4=tst")
        machine.fail("ip link show waywarp5")
        machine.succeed("! ip -4 rule show pref 32000 | grep -F waywarp5")
        machine.succeed("! ip -6 rule show pref 32000 | grep -F waywarp5")

    with subtest("instances are locked while running"):
        machine.fail("waywarp up proxy home --listen 127.0.0.1:2000")
        machine.fail("waywarp import tokyo")

    with subtest("stopping an instance removes what it created"):
        machine.succeed("systemctl stop waywarp-tokyo.service")
        machine.fail("ip link show waywarp2")
        machine.succeed("test -z \"$(ip -4 rule show pref 32000; ip -6 rule show pref 32000)\"")
        machine.succeed("waywarp down home")
        machine.wait_until_fails("systemctl is-active waywarp-home.service")
        machine.succeed("waywarp status | grep -Fx 'no running instances'")
  '';
}
