{ self, warp-stub }:
{
  name = "waywarp";
  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];
      environment.systemPackages = [ pkgs.socat ];
      boot.kernelModules = [ "dummy" ];
      services.waywarp = {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default.override {
          cloudflare-warp = warp-stub;
        };
        instances = {
          home = {
            index = 0;
            access.proxy = { };
          };
          tokyo = {
            index = 2;
            access.bridge.subnet4 = "10.9.0.4/30";
            location = "edge=tst";
          };
        };
      };
    };

  testScript = ''
    machine.wait_for_unit("waywarp-home.service")
    machine.wait_for_unit("waywarp-tokyo.service")

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

    with subtest("instances are locked while running"):
        machine.fail("waywarp up proxy home --listen 127.0.0.1:2000")
        machine.fail("waywarp import tokyo")

    with subtest("stopping an instance removes what it created"):
        machine.succeed("systemctl stop waywarp-tokyo.service")
        machine.fail("ip link show waywarp2")
        machine.succeed("waywarp down home")
        machine.wait_until_fails("systemctl is-active waywarp-home.service")
        machine.succeed("waywarp status | grep -Fx 'no running instances'")
  '';
}
