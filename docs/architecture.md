# Architecture

This document describes how Waywarp isolates and controls WARP instances. The [README](../README.md) covers usage.

## Terms

| Term | Meaning |
| --- | --- |
| instance | One isolated WARP client, keyed by an index from 0 to 255, optionally named |
| selector | An index or a name, as commands accept them |
| access | How the host reaches an instance: `proxy` or `bridge` |
| interface | The physical network interface that carries every packet leaving an instance |
| via | One entry of the ordered list of ways to reach the edge: direct, a SOCKS5 relay, or Mudfish nodes |
| route | One concrete way to connect expanded from a via: direct, or through one relay |
| relay | A SOCKS5 server used only while bootstrapping |
| bootstrap | Connecting WARP along each route in turn until the tunnel satisfies the required locations |
| migrate | Moving every flow off the relay onto its direct socket, keeping the QUIC session |
| ping | Screening a relay before WARP uses it |
| locations | The observed location fields; `--location` constrains them |
| supervisor | The process that owns an instance for its whole life |

## Processes

`waywarp up` validates options, takes the instance lock, and prepares the instance's files. Then:

- Detached, it starts a supervisor in a new session with the hidden `__supervise` command. The plan, the lock, and in proxy access the already bound listener travel over a `SOCK_SEQPACKET` pair. The supervisor reports progress and the result over the same channel. If `up` exits or is interrupted before setup finishes, the supervisor exits and the kernel releases everything it created.
- With `--foreground`, the command becomes the supervisor itself, logs to stderr, and notifies systemd when ready.

After setup, the supervisor serves `status`, `warp-cli`, and `down` on a control socket in the runtime directory.

The instance lock is an exclusive `flock` in the runtime directory. It belongs to the open file, so it follows the descriptor into a detached supervisor and lasts until the instance ends. `up` and `import` take it before changing anything, which makes it the single answer to whether an instance is running or starting. Names live in a `name` file in each instance's state directory, and a store-wide lock serializes their assignment.

## Isolation

The supervisor enters a private mount namespace before starting any thread. For normal users, it first creates a user namespace that maps the caller to root.

The mount namespace gives `warp-svc` private copies of every path it writes:

| WARP path | Instance path |
| --- | --- |
| `/var/lib/cloudflare-warp` | `STATE/INDEX/registration` |
| `/var/log/cloudflare-warp` | `STATE/INDEX/logs` |
| `/run/cloudflare-warp` | `RUNTIME/INDEX/daemon` |
| `/etc/resolv.conf` | `RUNTIME/INDEX/resolv.conf` |

`warp-svc` runs `/usr/sbin/ip` and `/usr/sbin/nft` by absolute path. Where they are missing, as on NixOS, an overlay on `/usr` adds them from `PATH`.

`warp-svc` runs in a private network namespace. Its only default route is a TUN device owned by the supervisor, so it cannot reach the network on its own.

Helpers die with the thread that started them, and unblock the termination signals the supervisor blocks for its signal thread.

## Data plane

One event loop per instance carries all traffic between the namespace and the host.

- UDP from `warp-svc` arrives on the TUN. Each flow gets a socket bound to the physical interface, so host routes and host VPNs are bypassed. Unless `--interface` pins it, the interface is resolved through host routing for each new flow, so a network change takes effect when WARP reconnects.
- During bootstrap, new UDP flows go through a SOCKS5 association instead.
- Team registrations also need direct TCP for policy refreshes, posture requests, DNS-over-HTTPS, and connectivity checks. An nftables output redirect sends locally originated TCP to a namespace listener. The event loop reads `SO_ORIGINAL_DST`, opens a host socket bound to the physical interface, and splices the two streams. Consumer registrations retain the UDP-only path.
- In proxy access, connections accepted on the host listener are spliced to connections opened inside the namespace to the WARP proxy port.
- In bridge access, a dual-stack veth pair connects the host to WARP. Namespace-local nftables permits both directions, masquerades outbound traffic, and maps unsolicited traffic for WARP's own addresses to the host-side link addresses. Separate IPv4 and IPv6 policy tables route host ingress to WARP and WARP ingress to the host. No host nftables rules are installed.

New flows are blocked until bootstrap chooses a route, and again between attempts, so WARP cannot connect along a route nobody chose.

## Bootstrap

Each bootstrap expands `--via` into routes, fetching the Mudfish node list afresh, and tries them in order.

1. Consecutive relays are pinged four at a time. A ping sends a QUIC version negotiation packet through the relay and measures the reply. It also asks Cloudflare which colo serves the relay; relays whose colo cannot satisfy `--location` are tried last.
2. WARP connects along one route at a time. Each attempt starts from a confirmed disconnect, so an earlier session cannot carry over.
3. After a relayed connection succeeds, the data plane migrates every flow to its direct socket. QUIC connection migration keeps the session and its location.
4. The supervisor reads the location fields, from inside the namespace, and checks the constraints. A mismatch moves on to the next route.

## Monitoring

A persistent `warp-cli --listen status` process reports state changes, so waits react to events instead of polling. The daemon's readiness is the only polled wait, because `warp-svc` announces nothing when its socket opens.

After every later `Connected` event, the supervisor reads the location fields again and compares them with the constraints. If one no longer matches and rebootstrap is enabled, it bootstraps again.

The instance stops when `warp-svc` exits or the data plane fails. It does not restart itself; under systemd, the unit's restart policy does.

## Source layout

| Module | Responsibility |
| --- | --- |
| `cli` | Command-line interface |
| `client` | Commands that start instances or talk to supervisors |
| `store` | State and runtime directories, names, selectors, and locks |
| `supervise` | Supervisor lifecycle, bootstrap, and control socket |
| `warp` | `warp-cli` commands, the `warp-svc` daemon, and the state monitor |
| `dataplane` | Event loop, UDP flows, TCP splices, and packet framing |
| `via` | Via parsing and expansion, physical interface, SOCKS5, Mudfish, and pings |
| `location` | Location constraints, Cloudflare geofeed, and location probes |
| `sandbox` | Private mount and network namespaces, TUN, and TCP redirect |
| `bridge` | Bridge link, subnets, firewall, and routes |
| `http` | Shared HTTP client with SOCKS5 and pinned addresses |
| `tool` | Running `ip`, `nft`, and other helpers |
| `notify` | systemd readiness notification |
| `text` | Text normalization shared by filters and places |
| `ipc`, `protocol` | Messages between client and supervisor |
