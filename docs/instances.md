# Instances

An instance is a WARP client with its own registration, daemon, and network namespace. Its index (`0`–`255`) determines its default proxy port, bridge link, and subnets. `up`, `down`, and `import` default to index `0`; `status` without a selector lists all running instances.

Root and each user have separate stores. If you start an instance with `sudo`, use `sudo` for its other commands too.

## Names and lifecycle

```sh
waywarp up proxy 1 --name home --accept-tos
waywarp status home
waywarp warp-cli home tunnel stats
waywarp down home
```

New registrations require `--accept-tos` after reviewing [Cloudflare's terms](https://www.cloudflare.com/application/terms/). Saved and imported registrations do not. Missing consent exits `2` before starting a daemon or entering namespaces.

Names are optional aliases for indices and must be unique within a store. They can contain up to 32 lowercase letters, digits, and hyphens, starting with a letter. `--name` replaces the saved alias without changing the index or registration; the old name stops resolving.

Stopping keeps the registration and name, **but not command-line options**. Repeat the access mode, location requirements, and relay options when starting again.

## Status and control

`waywarp status [INSTANCE]` reports connection health and [locations](locations.md). Human output uses aligned rows; scripts should use `status --json`, which emits one JSON object per line, not an array.

Status exits `0` only if every reported instance is currently verified and meets its location requirements. It exits `1` for unhealthy results or query errors. With no running instances, it exits `0`, printing `no running instances` in human mode and nothing in JSON mode.

A connected tunnel awaiting fresh verification is degraded; its location fields retain the last verified observations. This health check does not test every destination. Bridge NAT validity and translation targets are reported separately and do not determine the health exit code.

Startup and status results go to stdout; diagnostics go to stderr. Successful `down` and `import` commands report on stderr only. Waywarp disables its color on redirected streams, with nonempty `NO_COLOR`, or with `TERM=dumb`.

`waywarp warp-cli INSTANCE ...` forwards arguments, stdout, stderr, and the native exit code unchanged. Waywarp does not add consent: pass it explicitly if needed, as in `waywarp warp-cli INSTANCE --accept-tos registration new`.

## Service managers

Use foreground mode when a service manager should own the process:

```sh
waywarp up proxy 1 --name home --foreground
```

It prints one readiness block to stdout, logs to stderr, stops on SIGTERM, and signals systemd through `NOTIFY_SOCKET` after connection and location checks pass. Runtime failures stop the instance; use a service manager or the [NixOS module](nixos.md) to restart it.

## Networks

Unless you set `--interface NAME`, each new tunnel flow uses the physical interface selected by host routing. Existing sockets do not move when routes change; new flows usually follow the change when WARP reconnects.

Waywarp bypasses host VPNs rather than using them as relays. Automatic selection can fail when the host route uses a virtual link; choose the physical link explicitly with `--interface`.

## Setup and troubleshooting

- Install `warp-svc`, `warp-cli`, `ip`, and `nft`, and check that `/dev/net/tun` exists. The Nix package includes the runtime tools.
- Rootless proxy access needs unprivileged user namespaces and `XDG_RUNTIME_DIR`. If user namespaces are restricted, use root.
- WARP's `/var/lib/cloudflare-warp`, `/var/log/cloudflare-warp`, and `/run/cloudflare-warp` directories must exist as mount targets. Root-owned runs can create them; for rootless runs, create missing directories as root or start the host WARP service once. That service need not stay running.
- Slow Mudfish searches may be waiting between logins. Narrow the [node filter](regions.md#mudfish-filters).
- After a network manager reload, check whether it removed the bridge's [routing rules](access.md#routing).

## Logs

Detached runs replace their owner-only log on each start:

| Owner | Log |
| --- | --- |
| Normal user | `$XDG_STATE_HOME/waywarp/INDEX/waywarp.log`, defaulting to `~/.local/state/waywarp/INDEX/waywarp.log` |
| Root | `/var/lib/waywarp/INDEX/waywarp.log` |

Foreground logs go to stderr, or the journal under systemd. Detached logs include timestamps and structured instance context. Commands default to info for `waywarp::lifecycle` and warn otherwise; supervisors default to info.

`WAYWARP_LOG` overrides filtering for the command and supervisor without suppressing status or JSON results. Relay probe details are debug-level. Per-target overrides are supported:

```sh
WAYWARP_LOG=debug waywarp up proxy --accept-tos
WAYWARP_LOG=info,waywarp::dataplane=trace waywarp up proxy --accept-tos
```

Registrations contain credentials. Do not include their contents in a bug report.
