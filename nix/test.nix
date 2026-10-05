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
          doCheck = false;
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

  testScript = builtins.readFile ./tests/system.py;
}
