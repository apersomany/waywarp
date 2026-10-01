# NixOS

The flake includes a NixOS module for running root-owned Waywarp instances as systemd services.

## Setup

Add `waywarp.url = "github:apersomany/waywarp";` to your flake inputs. Pass `inputs` through `specialArgs` in your `nixosSystem`, then import the module in your configuration:

```nix
{ inputs, ... }: {
  imports = [ inputs.waywarp.nixosModules.default ];
  services.waywarp.instances = {
    home = {
      index = 0;
      acceptTos = true;
      access.proxy = { };
    };
    hong-kong = {
      index = 2;
      acceptTos = true;
      access.bridge = { };
      location = "geo4=HK";
      via = [ "mudfish:city=hongkong" ];
      environmentFile = "/run/secrets/waywarp-mudfish";
    };
  };
}
```

`acceptTos = true` accepts [Cloudflare's WARP Terms of Service](https://www.cloudflare.com/application/terms/) for a new registration. Review them before enabling it. The default is `false`; saved and imported registrations can start without it.

This creates `waywarp-home.service` and `waywarp-hong-kong.service` and installs the `waywarp` command. Use `sudo` to inspect these root-owned instances:

```sh
sudo waywarp status hong-kong
journalctl -u waywarp-hong-kong
```

The module does not start the host's `cloudflare-warp` service, and does not need it to be running.

## Options

Each entry in `services.waywarp.instances` defines one service. Its attribute name becomes the instance's name and must follow the [instance naming rules](instances.md#names-and-lifecycle).

The `index` remains the instance's primary key. Changing an attribute name while keeping the index reuses that instance's registration. Changing the index selects a different instance. Every configured instance needs a unique index.

| Option | Meaning |
| --- | --- |
| `index` | Required index from `0` to `255` |
| `acceptTos` | Accept Cloudflare's WARP terms when creating a registration; defaults to `false` |
| `access.proxy.listen` | IPv4 loopback listen address; default port is `1080 + index` |
| `access.bridge.subnet4`, `access.bridge.subnet6` | Override the derived bridge subnets |
| `access.bridge.nat` | `"auto"` (default), `"always"`, or `"never"`; see [bridge NAT](access.md#firewall-and-nat) |
| `location` | Location requirements, as for `--location` |
| `via` | Ordered connection paths, as for repeated `--via` |
| `interface` | Physical interface, as for `--interface` |
| `edge`, `edgePort` | MASQUE endpoint address and port, as for `--edge` and `--edge-port` |
| `mudfishPort` | Mudfish SOCKS5 port, as for `--mudfish-port` |
| `rebootstrap` | Set to `false` to keep a connection whose location no longer matches |
| `environmentFile` | Absolute path to a systemd environment file containing relay credentials |

Choose exactly one of `access.proxy` or `access.bridge`. An empty set, such as `access.proxy = { };`, uses that mode's defaults.

### Credentials

`environmentFile` must point outside the Nix store. A file supplied by a secret manager at boot is a suitable choice. It contains lines such as:

```text
WAYWARP_MUDFISH_USERNAME=...
WAYWARP_MUDFISH_PASSWORD=...
```

The file must be readable by the root-owned service. Do not put relay passwords in the Nix configuration or store.

### Package

`services.waywarp.package` selects the package. By default, it uses the flake's own nixpkgs and permits the unfree headless WARP package for that build only.

If you supply a package built from your own nixpkgs, your unfree policy must allow `cloudflare-warp-headless`.

## Service behavior

Each service runs the selected access mode with `--foreground`. For example, the proxy service above runs `waywarp up proxy 0 --name home --foreground --accept-tos`.

The service reports ready once WARP is connected and its required locations match. Other units can order themselves after it. Logs go to the journal.

Runtime failures restart the service after five seconds. Invalid command-line options or missing consent for a new registration exit with code `2`, which the module does not retry. The start timeout is ten minutes, allowing time for paced relay searches; the stop timeout is thirty seconds.
