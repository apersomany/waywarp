# Instances

An instance is a WARP client with its own registration, daemon, and network namespace. Its index, from `0` to `255`, is its primary key. The index also determines the default proxy port, bridge link, and subnets. Commands that accept an optional index use `0` when you leave it out.

Root and each user have separate stores. If you start an instance with `sudo`, use `sudo` for its other commands too.

## Names and lifecycle

```sh
waywarp up proxy 1 --name home --accept-tos
waywarp status home
waywarp warp-cli home tunnel stats
waywarp down home
```

For a new registration, review [Cloudflare's Terms of Service](https://www.cloudflare.com/application/terms/) and pass `--accept-tos` to agree. Without the flag, startup exits with code `2` before starting a daemon or entering namespaces. Saved and imported registrations do not need the flag again.

A name is an optional alias for an index, not a separate instance. You can use either in later commands. `--name` replaces the saved name without changing the index or registration; the old name then stops resolving.

Names must be unique within a store. They can contain up to 32 lowercase letters, digits, and hyphens, and must start with a letter.

Stopping an instance keeps its registration and name. Starting it again reuses those, **but not its previous command-line options**. Repeat the access mode, location requirements, and relay options each time.

## Status and control

`waywarp status` lists running instances. Pass an index or name to check just one. Human output is a compact, aligned table: `Instance` contains `INDEX (NAME)`, or just `INDEX` when unnamed, and `Access` contains `ADDRESS (proxy)` or `LINK (bridge)`. Both the `Instance` key and value are colored on suitable terminals. See [terminal output](terminal-output.md) for examples, color controls, and stream contracts.

`status --json` prints one JSON object per instance, rather than a JSON array. Status results go to stdout; diagnostics go to stderr.

The command exits `1` unless every reported instance has a currently verified connection and meets its location requirements. A connected tunnel awaiting fresh verification is reported as degraded; its location fields retain the last verified observations. This is useful as a connection health check, but it does not test reachability to every destination. Bridge NAT targets and configuration validity are reported separately.

`waywarp warp-cli INSTANCE ...` runs `warp-cli` against that instance's daemon with your arguments unchanged. If a command requires terms acceptance, pass it explicitly, for example `waywarp warp-cli INSTANCE --accept-tos registration new`. Waywarp does not add the flag to these calls. `down` stops the instance without deleting its registration or name.

## Service managers

Use foreground mode when a service manager should own the process:

```sh
waywarp up proxy 1 --name home --foreground
```

It prints one readiness table to stdout, logs lifecycle messages and diagnostics to stderr, stops on SIGTERM, and signals systemd readiness through `NOTIFY_SOCKET`. Ready means WARP is connected and the requested locations match. A runtime failure stops the instance; Waywarp does not restart itself. Let the service manager handle that, or use the [NixOS module](nixos.md).

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

Detached runs replace the previous log on each start. The log is owner-only and includes timestamps, levels, thread names, and targets; lifecycle records retain structured instance context:

| Owner | Log |
| --- | --- |
| Normal user | `$XDG_STATE_HOME/waywarp/INDEX/waywarp.log`, or `~/.local/state/waywarp/INDEX/waywarp.log` by default |
| Root | `/var/lib/waywarp/INDEX/waywarp.log` |

Foreground runs log to stderr, which goes to the journal under systemd. Console lifecycle messages omit instance identity, for example `info: connecting directly`. Commands default to info for the `waywarp::lifecycle` target and warn for other diagnostics; supervisors default to info. These defaults also apply when stderr is redirected.

Set `WAYWARP_LOG` to override filtering for the command and its supervisor, including lifecycle notices. It does not suppress status or JSON results. Per-target overrides are also supported:

```sh
WAYWARP_LOG=debug waywarp up proxy --accept-tos
WAYWARP_LOG=info,waywarp::dataplane=trace waywarp up proxy --accept-tos
WAYWARP_LOG=warn,waywarp::lifecycle=info waywarp up proxy --accept-tos
```

Successful `down` and `import` commands also report through tracing on stderr; they write no stdout result.

Registrations contain credentials. Do not include their contents in a bug report.
