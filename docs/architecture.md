# Architecture

Waywarp runs the official WARP client inside private namespaces and controls how it reaches the network. This document covers the implementation; start with the [README](../README.md) for usage.

## Terms

| Term | Meaning |
| --- | --- |
| instance | One WARP client, keyed by an index from `0` to `255`, with an optional name |
| selector | An index or name accepted by a command |
| access | How the host uses an instance: `proxy` or `bridge` |
| interface | The host network interface used for outgoing tunnel and direct control traffic |
| via | One `--via` entry: direct, a SOCKS5 relay, or a Mudfish filter |
| route | A concrete connection path expanded from a via: direct or one relay |
| relay | A SOCKS5 server used during connection setup |
| bootstrap | Trying connection paths until WARP meets the location requirements |
| migrate | Moving outer UDP flows from the relay to direct sockets |
| ping | Testing a relay before asking WARP to connect through it |
| locations | The observed fields checked by `--location` |
| supervisor | The process that owns and runs an instance |

## Processes and locks

`waywarp up` validates its options and locks the instance. If there is no saved registration, it requires `--accept-tos` before proceeding with setup. The consent flag is passed to the supervisor and checked again before starting the registration daemon. In proxy mode, it binds the host listener before starting setup so an address conflict fails early.

By default, `up` starts a detached supervisor in a new session using the hidden `__supervise` command. It sends the setup plan over a `SOCK_SEQPACKET` socket pair, along with the open lock and proxy listener descriptors. The supervisor reports progress and the final result over the same channel. If the caller exits or is interrupted before setup finishes, the supervisor exits too, and the kernel releases its resources.

With `--foreground`, the command runs the supervisor itself. It logs to stderr and sends systemd a readiness notification after the connection and location checks succeed.

Once setup is complete, a control socket in the runtime directory serves `status`, `warp-cli`, and `down`.

The instance lock is an exclusive `flock`. It stays held through the open descriptor passed to the supervisor, until the instance stops. Both `up` and `import` take this lock before changing state, so they cannot modify a running or starting instance.

Indices are primary keys. Names are aliases saved in a `name` file under each instance's state directory. A store-wide lock prevents two instances from being assigned the same name.

## Isolation

The supervisor creates its mount namespace before starting threads. For a normal user, it first creates a user namespace that maps the caller to root inside it.

Bind mounts give `warp-svc` its own state, logs, sockets, and DNS configuration:

| WARP path | Instance path |
| --- | --- |
| `/var/lib/cloudflare-warp` | `STATE/INDEX/registration` |
| `/var/log/cloudflare-warp` | `STATE/INDEX/logs` |
| `/run/cloudflare-warp` | `RUNTIME/INDEX/daemon` |
| `/etc/resolv.conf` | `RUNTIME/INDEX/resolv.conf` |

WARP calls `/usr/sbin/ip` and `/usr/sbin/nft` by absolute path. If those paths are missing, as on NixOS, an overlay on `/usr` supplies the tools from `PATH` without hiding the rest of `/usr`.

An empty tmpfs hides `/run/nscd`. Otherwise, glibc can ask the host's nscd to resolve names on WARP's behalf. Those lookups would use the host network rather than the DNS proxy in the private `resolv.conf`.

The running daemon has its own network namespace. Before WARP connects, its only default route leads to a supervisor-owned TUN device. Waywarp decides which traffic from that device can leave the namespace.

Initial device registration needs TCP, so it uses a temporary daemon on the host network. That daemon has its capabilities stripped to keep it from changing host routes or firewall state. It uses the instance's private state and is stopped before the regular daemon starts.

Helper processes receive a parent-death signal if their owning thread dies. They also unblock the termination signals reserved for the supervisor's signal-handling thread.

## Data plane

One event loop per instance handles UDP forwarding and TCP connections between the private namespace and host sockets.

### Tunnel UDP

UDP packets from `warp-svc` arrive on the supervisor's TUN. Each flow gets a host socket bound to the chosen interface. With automatic selection, Waywarp resolves a physical interface through host routing for each new flow. Binding to that interface keeps tunnel traffic out of host VPNs. `--interface` fixes the choice instead.

During relay setup, UDP sent to the configured WARP edge uses SOCKS5 associations. Other underlay UDP, including DNS queries needed to validate proxy mode, stays direct. Relaying those queries would give every DNS flow a separately paced Mudfish login and stall validation.

After a successful connection, migration switches the tunnel flows to their direct sockets and closes the relay associations. Only a reply from the configured WARP edge confirms migration; an unrelated DNS reply does not.

New flows are blocked before a connection path is selected and between attempts. WARP therefore cannot make a new UDP connection over an unintended path.

### Direct TCP for Team registrations

Team registrations also need direct TCP for policy refreshes, posture requests, DNS-over-HTTPS, and connectivity checks.

An nftables output redirect catches locally originated IPv4 TCP routed through the emulated physical uplink, `tun`, and sends it to a listener inside the namespace. The event loop reads `SO_ORIGINAL_DST`, opens a host socket bound to the chosen interface, and copies data between the streams.

TCP routed through `CloudflareWARP` is left alone. Sending it directly would bypass the tunnel, including for connector control traffic. Consumer registrations use the UDP-only underlay path.

### Host access

Proxy mode accepts connections on the host listener and forwards them to WARP's proxy port inside the namespace. TCP forwarding uses userspace buffers, not the kernel's `splice(2)` syscall.

Bridge mode connects the host and namespace with an IPv4/IPv6 veth pair. Namespace-local nftables permits both directions. Source NAT uses the assigned device addresses, except for sources the configured policy leaves untranslated. Incoming traffic for those device addresses maps to the host-side link addresses.

Separate IPv4 and IPv6 policy tables route traffic from the host into WARP and traffic from WARP back to the host. No host nftables rules are installed. See [bridge access](access.md) for the routing and NAT policy.

## Relay authentication

All Mudfish SOCKS5 handshakes in a store share an authentication limiter. This includes relay pings, colo traces, and tunnel flows.

A locked, owner-only file in the runtime directory ensures exchanges do not overlap and leaves at least six seconds between them, including failures. It holds only a boot-time monotonic timestamp, not credentials or account identifiers. The timestamp survives supervisor restarts. Its path is resolved before entering a rootless user namespace.

Other users' stores and external programs do not share this limit. Ordinary `socks5:` entries are not paced.

## Bootstrap

Each setup expands the ordered `--via` entries into connection paths, fetching the Mudfish node list again when needed.

1. Consecutive relays are pinged four at a time. A QUIC version negotiation packet tests UDP reachability and measures the reply time. A separate trace request gives a colo hint. Relays that look unlikely to meet the requirements are tried last, not discarded: the hint may be wrong.
2. Each connection attempt starts from a confirmed disconnect. The UDP routing state is blocked, direct, or relayed, so an earlier connection cannot carry over into the next attempt.
3. For a relay, the supervisor authenticates the first UDP association on the host before starting WARP and hands it to the event loop. This keeps Mudfish's pacing delay outside WARP's short Happy Eyeballs deadline. Later flows waiting for an association retain up to 32 datagrams and 64 KiB in FIFO order. Overflow drops new arrivals rather than replacing the initial QUIC packets.
4. After WARP connects through a relay, Waywarp switches its UDP flows to direct sockets. WARP may migrate the existing QUIC session or reconnect while the new path settles. The supervisor waits for a connection if that happens.
5. The supervisor reads the locations from inside the namespace and checks every requirement. A mismatch moves on to the next connection path.

The observations behind location retention are described separately in [WARP routing observations](routing.md).

## Monitoring and cleanup

A persistent `warp-cli --listen status` process reports connection changes. Connection waits use those events rather than polling. Daemon startup is polled because `warp-svc` does not announce when its control socket opens.

After a later `Connected` event, the supervisor reads the location fields again. If a required field no longer matches and rebootstrap is enabled, it repeats setup.

### Bridge updates

WARP recreates its link on connection, and the kernel removes routes through the old link. The supervisor restores the bridge route after setup and before and after each reconnect check. It also checks the bridge when `conf.json` changes and every five seconds.

Each update takes one snapshot of WARP's link and applies the derived changes under one lock:

- Restore the host-to-WARP route. The return route and rule use an interface name rather than WARP's link index, so they are installed once.
- Set WARP's MTU on both ends of the veth pair. This lets the host report oversized packets itself; errors generated inside the namespace could follow WARP's routes away from the host. The firewall also clamps TCP MSS in both directions.
- Read the NAT mode and `conf.json`, verify the assigned addresses against the link, and replace changed NAT rules in one nftables transaction.

In `auto` mode, advertised connector source subnets are left untranslated when `nat_mode` is false. Other sources are translated to `interface.v4` or `interface.v6`. Inbound DNAT matches only these assigned addresses, leaving shared connector addresses such as mesh DNS inside the namespace.

Translation targets must be on WARP's current link. Traffic needing translation is blocked for a family without a valid target. Invalid configuration removes the untranslated-source exceptions and uses the last verified targets, checked against the current link. The directory watch catches atomic replacement of `conf.json`. Conntrack is not flushed, and registration contents are never logged.

### Shutdown

The host link and policy rules belong to an attachment that removes them on drop, including if setup fails later. The kernel also releases namespace resources when the supervisor exits.

An instance stops if `warp-svc` exits or the data plane fails. It does not restart itself; a service manager can restart it.

## Source layout

| Module | Responsibility |
| --- | --- |
| `cli` | Command-line interface |
| `client` | Starting instances and sending control requests |
| `store` | State and runtime directories, names, selectors, and locks |
| `supervise` | Instance lifecycle, bootstrap, bridge updates, and control socket |
| `warp` | `warp-cli` commands, daemon management, and status monitoring |
| `dataplane` | Event loop, UDP flows, TCP forwarding, and packet framing |
| `via` | Connection paths, interfaces, SOCKS5, Mudfish, and relay pings |
| `location` | Location requirements, geofeed data, and probes |
| `sandbox` | Mount and network namespaces, TUN, and TCP redirect |
| `bridge` | Host link, subnets, firewall, routes, link snapshots, and NAT |
| `http` | HTTP requests with SOCKS5 and pinned addresses |
| `tool` | Running `ip`, `nft`, and other helpers |
| `notify` | systemd readiness notification |
| `text` | Text normalization for filters and places |
| `ipc`, `protocol` | Messages and descriptor passing between client and supervisor |
