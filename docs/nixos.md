# NixOS

The flake provides a NixOS module that runs root-owned instances as systemd services. Add the flake as an input, pass `inputs` through `specialArgs` in your `nixosSystem`, and import the module:

```nix
# In your flake inputs: waywarp.url = "github:apersomany/waywarp";
{ inputs, ... }: {
  imports = [ inputs.waywarp.nixosModules.default ];
  services.waywarp.instances = {
    home = {
      index = 0;
      access.proxy = { };
    };
    hong-kong = {
      index = 2;
      access.bridge = { };
      location = "geo4=HK";
      via = [ "mudfish:city=hongkong" ];
      environmentFile = "/run/secrets/waywarp-mudfish";
    };
  };
}
```

This creates `waywarp-home.service` and `waywarp-hong-kong.service` and installs `waywarp`, so `sudo waywarp status hong-kong` works.

## Options

Each attribute of `services.waywarp.instances` is one instance, and its name is the instance name. Names follow the same rules as `--name`.

| Option | Meaning |
| --- | --- |
| `index` | The instance index, required and unique |
| `access.proxy.listen` | Proxy listen address |
| `access.bridge.subnet4`, `access.bridge.subnet6` | Bridge subnets |
| `access.bridge.nat` | `"auto"` (default), `"always"`, or `"never"`, as for `--nat` |
| `location` | As for `--location` |
| `via` | A list of vias, as for repeated `--via` |
| `interface`, `edge`, `edgePort`, `mudfishPort` | As for the matching flags |
| `rebootstrap` | `false` is `--no-rebootstrap` |
| `environmentFile` | A systemd environment file with relay credentials |

`access` takes exactly one of `proxy` or `bridge`. Write `access.proxy = { };` to use the defaults.

`environmentFile` must be an absolute path outside the Nix store, such as a file your secret manager creates at boot. It holds lines like `WAYWARP_MUDFISH_USERNAME=...` and `WAYWARP_MUDFISH_PASSWORD=...`.

`services.waywarp.package` picks the package. The default comes from the flake's own nixpkgs, which allows the unfree headless WARP package for that build only. A package from your own nixpkgs needs your unfree policy to allow `cloudflare-warp-headless`.

## Services

Units run `waywarp up --foreground` and report ready once WARP is connected at the required locations, so other units can order themselves after them. Logs go to the journal:

```sh
journalctl -u waywarp-hong-kong
```

A unit that fails at runtime restarts after five seconds. A unit whose options are invalid fails once and stays failed.

The module does not start the host's `cloudflare-warp` service, and does not need it.
