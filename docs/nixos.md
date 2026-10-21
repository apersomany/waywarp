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

The module installs `waywarp` and creates `waywarp-home.service` and `waywarp-hong-kong.service`. It neither starts nor requires the host's `cloudflare-warp` service. Inspect these root-owned instances with `sudo`:

```sh
sudo waywarp status hong-kong
journalctl -u waywarp-hong-kong
```

## Options

Each `services.waywarp.instances` entry defines a service. Its attribute name becomes the instance's name and must follow the [naming rules](instances.md#names-and-lifecycle). Indices must be unique; renaming an entry preserves its registration if the index stays the same.

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

Use a root-readable file outside the Nix store, such as one supplied by a secret manager at boot. It contains lines such as:

```text
WAYWARP_MUDFISH_USERNAME=...
WAYWARP_MUDFISH_PASSWORD=...
```

Do not put relay passwords in the Nix configuration or store.

### Package

`services.waywarp.package` defaults to the flake's package, built with its own nixpkgs and permission for the unfree headless WARP dependency. If you build a replacement from your own nixpkgs, allow `cloudflare-warp-headless` in your unfree policy.

## Service behavior

Services run with `--foreground` and report ready after connection and location checks pass, so other units can order themselves after them. Logs go to the journal.

Runtime failures restart after five seconds. Invalid options or missing registration consent exit `2` and are not retried. The start timeout is ten minutes for paced relay searches; the stop timeout is thirty seconds.
