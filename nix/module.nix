{ self }:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  inherit (lib) mkOption types;
  settings = config.services.waywarp;
  optional =
    flag: value:
    lib.optionals (value != null) [
      flag
      (toString value)
    ];
  instanceType = types.submodule {
    options = {
      index = mkOption {
        type = types.ints.u8;
        description = "Instance index, which derives the proxy port, bridge link, and subnets.";
      };
      access = mkOption {
        type = types.attrTag {
          proxy = mkOption {
            description = "Serve SOCKS5 and HTTP CONNECT on loopback.";
            type = types.submodule {
              options.listen = mkOption {
                type = types.nullOr types.str;
                default = null;
                example = "127.0.0.1:9050";
                description = "Listen address on loopback (defaults to 127.0.0.1:1080+index).";
              };
            };
          };
          bridge = mkOption {
            description = "Link the host to WARP with a dual-stack veth pair.";
            type = types.submodule {
              options = {
                subnet4 = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                  description = "IPv4 /30 subnet (defaults to one derived from the index).";
                };
                subnet6 = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                  description = "IPv6 /126 subnet (defaults to one derived from the index).";
                };
              };
            };
          };
        };
        example = {
          bridge = { };
        };
        description = "How the host reaches the instance: `proxy` or `bridge`, with their options.";
      };
      location = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "geo4=JP+edge=NRT";
        description = "Required locations, as for `waywarp up --location`.";
      };
      via = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "mudfish:city=tokyo" ];
        description = "Ways to reach the edge, tried in order (defaults to direct).";
      };
      interface = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Physical interface (defaults to whichever the host routes each destination through).";
      };
      edge = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "MASQUE edge IPv4 address.";
      };
      edgePort = mkOption {
        type = types.nullOr types.port;
        default = null;
        description = "MASQUE edge port.";
      };
      mudfishPort = mkOption {
        type = types.nullOr types.port;
        default = null;
        description = "SOCKS5 port of Mudfish nodes.";
      };
      rebootstrap = mkOption {
        type = types.bool;
        default = true;
        description = "Bootstrap again when a required location changes.";
      };
      environmentFile = mkOption {
        type = types.nullOr (
          types.pathWith {
            inStore = false;
            absolute = true;
          }
        );
        default = null;
        example = "/run/secrets/waywarp-mudfish";
        description = "Root-readable systemd EnvironmentFile with relay credentials, provided at runtime rather than through the Nix store.";
      };
    };
  };
  arguments =
    name: instance:
    let
      access = lib.head (lib.attrNames instance.access);
      options = instance.access.${access};
    in
    [
      "up"
      access
      (toString instance.index)
      "--name"
      name
      "--foreground"
    ]
    ++ optional "--location" instance.location
    ++ lib.concatMap (via: [
      "--via"
      via
    ]) instance.via
    ++ optional "--interface" instance.interface
    ++ optional "--edge" instance.edge
    ++ optional "--edge-port" instance.edgePort
    ++ optional "--mudfish-port" instance.mudfishPort
    ++ lib.optional (!instance.rebootstrap) "--no-rebootstrap"
    ++ optional "--listen" (options.listen or null)
    ++ optional "--subnet4" (options.subnet4 or null)
    ++ optional "--subnet6" (options.subnet6 or null);
  names = lib.attrNames settings.instances;
  indices = lib.mapAttrsToList (_: instance: instance.index) settings.instances;
in
{
  options.services.waywarp = {
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "waywarp.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "Waywarp package to run.";
    };
    instances = mkOption {
      type = types.attrsOf instanceType;
      default = { };
      example = {
        home = {
          index = 0;
          access.proxy = { };
        };
        tokyo = {
          index = 2;
          access.bridge = { };
          location = "geo4=JP";
          via = [ "mudfish:city=tokyo" ];
        };
      };
      description = "Root-owned Waywarp instances. Each attribute name is also the instance name, so `waywarp status tokyo` works.";
    };
  };

  config = lib.mkIf (settings.instances != { }) {
    assertions = [
      {
        assertion = lib.allUnique indices;
        message = "services.waywarp.instances: every instance needs its own index.";
      }
    ]
    ++ map (name: {
      assertion = builtins.match "[a-z][a-z0-9-]{0,31}" name != null;
      message = "services.waywarp.instances.${name}: names must be up to 32 lowercase letters, digits, and hyphens, starting with a letter.";
    }) names;

    environment.systemPackages = [ settings.package ];

    systemd.services = lib.mapAttrs' (
      name: instance:
      lib.nameValuePair "waywarp-${name}" {
        description = "Waywarp instance ${name}";
        wantedBy = [ "multi-user.target" ];
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        serviceConfig = {
          # Ready once WARP is connected at the required locations.
          Type = "notify";
          ExecStart = utils.escapeSystemdExecArgs (
            [ (lib.getExe settings.package) ] ++ arguments name instance
          );
          EnvironmentFile = lib.optional (instance.environmentFile != null) instance.environmentFile;
          Restart = "on-failure";
          # Invalid options exit with 2; retrying cannot fix them.
          RestartPreventExitStatus = 2;
          RestartSec = 5;
          TimeoutStartSec = "10min";
          TimeoutStopSec = 30;
        };
      }
    ) settings.instances;
  };
}
