# Changelog

Changes are listed here using [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

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
