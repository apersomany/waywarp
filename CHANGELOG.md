# Changelog

Changes are listed here using [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

## 0.1.3

### Changed

- Bridge namespace routing tables are selected from unused IDs, and rule priorities are derived from the kernel's local and main rules instead of fixed WARP priorities.

### Fixed

- Bridge replies to destinations covered by WARP-managed routes return to the host instead of reentering WARP, including IPv6 Mesh clients ([#1](https://github.com/apersomany/waywarp/issues/1)). Local connector services and locally originated WARP control traffic retain their routing behavior across reconnects.

## 0.1.2

### Changed

- Human status uses compact, aligned rows, including a colored `Instance` row and `ADDRESS (proxy)` or `LINK (bridge)` access values.
- Lifecycle messages, including `down` and `import` confirmations, use tracing on stderr with structured instance context retained in detached logs.
- Diagnostics use a shared renderer and preserve structured error causes. JSON status and native `warp-cli` output remain unchanged.
- Relay providers are expanded only when reached, so an unavailable later provider cannot prevent an earlier route from succeeding.
- CI combines package tests and Clippy; tagged releases also run the installer and VM integration checks before publishing.

### Fixed

- Missing-consent diagnostics end with a newline without changing Clap's styling, stream, or exit code.
- QUIC reconnects and kernel link changes trigger fresh tunnel verification. Stale location probes cannot mark a changed connection healthy.
- Bridge routes, MTU, and NAT are reconciled after link recreation and address changes, including changes without a CLI reconnect.
- Startup cancellation interrupts helper processes, paced relay authentication, and relay socket operations, then cleans up instance resources.
- Malformed or truncated IPC messages close received descriptors, and status errors are no longer silently treated as stopped instances.
- Registration imports recover interrupted replacements, and location cache updates are validated before replacing existing data.
- TCP forwarding handles nonblocking connection establishment, transient accept failures, and cancellation without stalling the packet loop.

## 0.1.1

### Changed

- New WARP registrations require explicit `--accept-tos` consent before setup. Saved and imported registrations can start without it.
- NixOS instances have an `acceptTos` option, defaulting to `false`.
- User-issued `warp-cli` commands no longer receive an implicit `--accept-tos` flag.

### Fixed

- Proxy setup through paced relays no longer stalls on underlay DNS queries. Only traffic to the configured WARP edge is relayed, and unrelated UDP replies cannot confirm tunnel migration.
- The lock-release test tolerates temporary descriptor inheritance while other tests spawn subprocesses.

## 0.1.0

### Added

- Separate WARP clients, each with its own registration, daemon, and network namespace. Indices are primary keys; optional unique names can be used in commands.
- Private DNS resolution for each client, without using the host's nscd.
- Regional connection setup through a relay, followed by migration onto the local network and location checks.
- SOCKS5 and HTTP CONNECT proxies on loopback, for normal users and root.
- Root-owned IPv4/IPv6 bridge links with derived `/30` and `/126` subnets. Bound sockets use WARP, while the host controls routing and filtering for other traffic.
- Bridge route, MTU, TCP MSS, and NAT updates after reconnects and registration changes.
- Bridge NAT that follows connector routes and uses the device's assigned addresses, with `always` and `never` overrides. Incoming traffic for those addresses reaches the host; shared connector services such as mesh DNS stay inside the namespace.
- Ordered `--via` paths: direct, SOCKS5 relays, and filtered Mudfish nodes. Each setup fetches a fresh Mudfish node list. Positive filter terms joined by `+` or `&` must all match; `-` excludes nodes.
- Mudfish login pacing shared by instances in a user's store. The first relay association is authenticated before WARP's handshake deadline starts.
- `--location` requirements for public IP geolocation, the tunnel colo, and IPv4/IPv6 probe colos. Locations are checked after reconnects, with automatic setup repeated when a requirement no longer matches.
- `status`, including JSON output, plus `down` and per-instance `warp-cli` commands.
- Foreground runs with systemd readiness notification.
- Zero Trust registration import without enrolling again, automatic Team edge selection, and direct TCP forwarding for requests routed outside the tunnel.
- Physical interface selection for each new flow, so new flows follow network changes.
- Logging levels and module overrides through `WAYWARP_LOG`.
- A Nix flake for x86_64 and aarch64 Linux, with a package, static release build, NixOS module, VM test, and development shell containing the runtime tools.
- CI, tagged releases with checksums and build provenance, and an installer script.
