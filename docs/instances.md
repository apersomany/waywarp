# Instances

An instance is one isolated WARP registration and daemon. Commands default to index `0`; indices `0`–`255` select independent registrations, proxy ports, bridge links, and subnets. Root and each normal user have separate instance stores: use the same privileges for every command on an instance.

## Names and lifecycle

```sh
waywarp up proxy 1 --name home
waywarp status home
waywarp warp-cli home tunnel stats
waywarp down home
```

`--name` replaces any previous name. Names remain usable after stopping an instance and must be unique within the store: up to 32 lowercase letters, digits, and hyphens, starting with a letter. Starting a saved instance reuses its registration, **not its previous command-line options**; repeat access, location, and relay options each time.

`waywarp status` lists running instances. `status --json` emits one JSON object per instance. It exits `1` if any reported instance is disconnected or fails its location constraints, making it a connection health check. Bridge configuration validity and NAT targets are reported separately; a connected status does not by itself prove every bridged destination is reachable.

`waywarp warp-cli INSTANCE ...` runs arbitrary `warp-cli` commands against that instance's daemon. `down` stops it but retains its registration and name.

## Service managers

`up --foreground` stays in the foreground, logs to stderr, signals systemd readiness through `NOTIFY_SOCKET`, and stops on SIGTERM. Readiness means WARP is connected at the requested locations. Runtime failures stop the instance; a service manager can restart it. See the [NixOS module](nixos.md).

## Networks

Unless `--interface` pins a link, each new tunnel flow uses the physical interface selected by host routing. A network change takes effect on new flows, normally when WARP reconnects. Host VPNs are bypassed rather than used as relays. If host routing selects a virtual link, Waywarp may require `--interface` to identify the physical uplink.

## Setup and troubleshooting

- Install `warp-svc`, `warp-cli`, `ip`, and `nft`, and ensure `/dev/net/tun` exists. The Nix package supplies the runtime tools.
- Normal-user proxy access needs unprivileged user namespaces and `XDG_RUNTIME_DIR`. Restricted distributions may require root instead.
- WARP's `/var/lib/cloudflare-warp`, `/var/log/cloudflare-warp`, and `/run/cloudflare-warp` mount targets must exist. The package or service normally creates them. If missing, create them once as root or start the WARP service once; Waywarp's error names the missing directory. Waywarp does not need the host service to remain running.
- Use `--interface NAME` when automatic physical-interface selection fails.
- A slow Mudfish search may be authentication pacing, not a hung instance; narrow the [filter](regions.md#mudfish-filters).
- For bridge failures after a network manager reload, check the [foreign-route policy](access.md#routing).

Detached runs keep their latest log at:

| Owner | Log |
| --- | --- |
| Normal user | `$XDG_STATE_HOME/waywarp/INDEX/waywarp.log`, defaulting to `~/.local/state/waywarp/INDEX/waywarp.log` |
| Root | `/var/lib/waywarp/INDEX/waywarp.log` |

Foreground runs log to stderr (the journal under systemd). `WAYWARP_LOG` accepts a level and optional per-module overrides for the command and its supervisor:

```sh
WAYWARP_LOG=debug waywarp up proxy
WAYWARP_LOG=info,waywarp::dataplane=trace waywarp up proxy
```

Registrations contain credentials. Do not publish their contents when reporting a problem.
