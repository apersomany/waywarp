# Architecture

Waywarp runs the official WARP client inside private namespaces and forwards its network traffic through host sockets. For usage, start with the [README](../README.md); this page is a map of the implementation.

## Processes and locks

Each instance has a supervisor that owns its daemon, data plane, and optional bridge. `up` validates options, checks registration consent, and takes an exclusive instance lock. Proxy mode binds its host listener before setup so address conflicts fail early. `import` takes the same lock and cannot modify a running or starting instance.

By default, `up` starts a detached supervisor in a new session. A `SOCK_SEQPACKET` socket pair carries the setup plan, lock and listener descriptors, progress, and the final result. If the caller disappears before setup completes, the supervisor stops. After setup, a runtime control socket serves `status`, `warp-cli`, and `down`.

With `--foreground`, the caller runs the supervisor itself and signals systemd readiness after connection and location checks pass. The instance lock stays held until shutdown. A separate store-wide lock protects name assignments.

## Isolation

The supervisor creates its mount namespace before starting threads. Rootless runs first create a user namespace mapping the caller to root inside it. Bind mounts give WARP private paths:

| WARP path | Instance path |
| --- | --- |
| `/var/lib/cloudflare-warp` | `STATE/INDEX/registration` |
| `/var/log/cloudflare-warp` | `STATE/INDEX/logs` |
| `/run/cloudflare-warp` | `RUNTIME/INDEX/daemon` |

An empty tmpfs hides `/run/nscd` so DNS lookups cannot escape through the host's resolver. A copy-on-write overlay on `/etc`, with its upper layer on a private tmpfs, gives WARP a private `/etc/resolv.conf` it can rename over and restore without touching the host's entry, including when the host's is a symlink such as systemd-resolved's. Where WARP's absolute `/usr/sbin/ip` and `/usr/sbin/nft` paths are missing, an overlay on `/usr` supplies them from `PATH`.

The running daemon has its own network namespace, with a supervisor-owned TUN as its physical uplink. Initial registration needs TCP, so a temporary daemon runs on the host network with capabilities stripped to prevent host route or firewall changes. It uses the private state and stops before the regular daemon starts. Helper processes receive a parent-death signal if their owning thread dies.

## Data plane

One event loop per instance handles UDP forwarding and TCP connections between the namespace and host sockets.

- **Tunnel UDP:** each flow gets a socket bound to the selected physical interface. During relay setup, only traffic to the configured WARP edge uses SOCKS5; other underlay UDP, including DNS, stays direct. Migration switches tunnel flows to direct sockets and closes relay associations. Only a reply from the configured edge confirms migration. New flows are blocked between connection attempts to prevent an unintended path.
- **Team control TCP:** namespace-local nftables redirects locally originated IPv4 TCP routed through the emulated uplink to a listener. The event loop reads `SO_ORIGINAL_DST` and forwards through a host socket bound to the selected interface. Traffic routed through `CloudflareWARP` stays in the tunnel, including connector control traffic.
- **Proxy access:** a host listener forwards connections to WARP's namespace-local proxy port using userspace buffers.
- **Bridge access:** an IPv4/IPv6 veth pair links the host and namespace. Namespace-local firewall and NAT rules handle both directions; Waywarp installs no host nftables rules. See [bridge access](access.md) for the user-facing policy.

Bridge routing allocates unused namespace table IDs. Ingress rules keep return traffic ahead of WARP-managed routes but after local delivery, preserving connector services. The return rule takes the local rule's priority and is installed before WARP starts, so it stays ahead of the policy WARP adds at the same priority however often WARP recreates it; a foreign policy already ahead of that position stops the instance rather than being moved. Host-to-WARP fallback comes after WARP's policies and before the namespace's physical uplink. These rules do not alter locally originated WARP control traffic.

## Bootstrap and verification

Setup expands ordered `--via` entries only when reached. Each entry yields a direct path or concrete relays; later providers are not queried after an earlier success.

1. Relays are pinged four at a time using QUIC version negotiation for UDP reachability and RTT, plus a trace request for a colo hint. Unlikely colos are tried last, not discarded.
2. Each attempt starts from a confirmed disconnect. The first relay association is authenticated before WARP connects, keeping Mudfish's pacing delay outside WARP's handshake deadline. Later associations use bounded FIFO queues for pending datagrams.
3. After a relayed connection succeeds, the data plane switches to direct sockets. WARP may migrate its QUIC session or reconnect; either way, the supervisor waits for a stable connection.
4. The supervisor reconciles the bridge, probes locations, and checks every requirement. Verification must stay on the same tunnel generation through probing and a quiet interval. After migration or an observed QUIC handshake, at least one tunnel probe must succeed.

A persistent `warp-cli --listen status` process reports connection changes. The data plane also observes QUIC handshakes without decrypting traffic. Handshake activity, path changes, CLI events, and bridge link changes invalidate verification, even if WARP still reports connected or reuses a connection-ID routing prefix.

A background worker verifies pending generations. Until a current result is published, a connected tunnel is degraded rather than inheriting earlier healthy status. A location mismatch repeats bootstrap unless disabled. See [regions and relays](regions.md) for migration behavior and the store-wide Mudfish authentication limit.

## Bridge updates and cleanup

Kernel link/address notifications, `conf.json` changes, tunnel verification, and a five-second fallback trigger bridge reconciliation. Lost or malformed link events force a fresh snapshot. Each update takes one WARP link snapshot under a lock, repairs policy rules and routes, copies its MTU to the veth pair, and atomically replaces changed NAT rules. TCP MSS is clamped in both directions.

Translation targets are verified against WARP's current link. Invalid configuration removes routed-source exceptions; traffic needing translation is blocked for a family without a valid target. Existing conntrack mappings are not flushed. Transient update races are retried; persistent reconciliation errors stop the instance.

The bridge attachment removes its host link and policy rules on drop, including setup failures. Kernel namespace resources disappear when the supervisor exits. An instance stops if its daemon exits or data plane fails; restart policy belongs to the service manager.

## Source map

| Module | Responsibility |
| --- | --- |
| [`cli`](../src/cli.rs), [`client`](../src/client.rs) | CLI validation, startup, and control requests |
| [`store`](../src/store.rs) | State/runtime paths, selectors, names, imports, and locks |
| [`supervise`](../src/supervise/mod.rs) | Lifecycle, bootstrap, verification, and bridge updates |
| [`warp`](../src/warp/mod.rs) | Daemon management, native commands, and status monitoring |
| [`sandbox`](../src/sandbox.rs) | Namespaces, bind mounts, TUN, and TCP redirect |
| [`dataplane`](../src/dataplane/mod.rs) | UDP/TCP forwarding and QUIC generation tracking |
| [`bridge`](../src/bridge.rs) | Links, routing, MTU, NAT, and kernel notifications |
| [`via`](../src/via/mod.rs) | Interfaces, SOCKS5, Mudfish, relay pings, and authentication pacing |
| [`location`](../src/location/mod.rs), [`http`](../src/http.rs) | Requirements, cached location data, and probes |
| [`output`](../src/output.rs) | Human results, diagnostics, stream handling, and tracing formatting |
| [`ipc`](../src/ipc.rs), [`protocol`](../src/protocol.rs) | Messages, structured progress/failures, and descriptor passing |
| [`tool`](../src/tool.rs), [`notify`](../src/notify.rs), [`text`](../src/text.rs) | Helper execution, systemd notifications, and text normalization |
