# Instances

An instance is a WARP client with its own registration, daemon, and network namespace. Its index, from `0` to `255`, is its primary key. The index also determines the default proxy port, bridge link, and subnets. Commands that accept an optional index use `0` when you leave it out.

Root and each user have separate stores. If you start an instance with `sudo`, use `sudo` for its other commands too.

## Names and lifecycle

```sh
waywarp up proxy 1 --name home
waywarp status home
waywarp warp-cli home tunnel stats
waywarp down home
```

A name is an optional alias for an index, not a separate instance. You can use either in later commands. `--name` replaces the saved name without changing the index or registration; the old name then stops resolving.

Names must be unique within a store. They can contain up to 32 lowercase letters, digits, and hyphens, and must start with a letter.

Stopping an instance keeps its registration and name. Starting it again reuses those, **but not its previous command-line options**. Repeat the access mode, location requirements, and relay options each time.

## Status and control

`waywarp status` lists running instances. Pass an index or name to check just one. `status --json` prints one JSON object per instance, rather than a JSON array.

The command exits `1` if any reported instance is disconnected or fails its location requirements. This is useful as a connection health check, but it does not test reachability to every destination. Bridge NAT targets and configuration validity are reported separately.

`waywarp warp-cli INSTANCE ...` runs `warp-cli` against that instance's daemon. `down` stops the instance without deleting its registration or name.

## Service managers

Use foreground mode when a service manager should own the process:

```sh
waywarp up proxy 1 --name home --foreground
```

It logs to stderr, stops on SIGTERM, and signals systemd readiness through `NOTIFY_SOCKET`. Ready means WARP is connected and the requested locations match. A runtime failure stops the instance; Waywarp does not restart itself. Let the service manager handle that, or use the [NixOS module](nixos.md).

## Networks

Unless you set `--interface`, each new tunnel flow uses the physical interface selected by host routing. Network changes affect new flows, usually when WARP reconnects. Existing sockets do not move to a new interface just because the route changes.

Waywarp bypasses host VPNs rather than using them as relays. If the host route points through a virtual link, automatic selection may fail. Use `--interface NAME` to choose the physical link explicitly.

## Setup and troubleshooting

- Install `warp-svc`, `warp-cli`, `ip`, and `nft`, and check that `/dev/net/tun` exists. The Nix package includes the runtime tools.
- Proxy access without root needs unprivileged user namespaces and `XDG_RUNTIME_DIR`. On systems that restrict user namespaces, use root instead.
- WARP's `/var/lib/cloudflare-warp`, `/var/log/cloudflare-warp`, and `/run/cloudflare-warp` directories must exist as mount targets. The package or service normally creates them. If one is missing, create it as root or start the WARP service once. Waywarp's error names the missing directory, and root-owned runs can create it during setup. The host service does not need to stay running.
- If automatic interface selection fails, pass `--interface NAME`.
- A slow Mudfish search may be waiting between logins, not hanging. Narrow the [node filter](regions.md#mudfish-filters).
- If a bridge stops working after a network manager reload, check whether the manager removed its [routing rules](access.md#routing).

## Logs

Detached runs write a fresh log for each start:

| Owner | Log |
| --- | --- |
| Normal user | `$XDG_STATE_HOME/waywarp/INDEX/waywarp.log`, or `~/.local/state/waywarp/INDEX/waywarp.log` by default |
| Root | `/var/lib/waywarp/INDEX/waywarp.log` |

Foreground runs log to stderr, which goes to the journal under systemd. Set `WAYWARP_LOG` to change the level for the command and its supervisor. Per-module overrides are also supported:

```sh
WAYWARP_LOG=debug waywarp up proxy
WAYWARP_LOG=info,waywarp::dataplane=trace waywarp up proxy
```

Registrations contain credentials. Do not include their contents in a bug report.
