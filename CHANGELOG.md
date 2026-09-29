# Changelog

All notable changes to this project are documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

- Isolated WARP instances identified by index, each with its own registration, daemon, and network namespace, and optionally by a unique name that every command accepts.
- Cross-regional exits: WARP bootstraps through a relay in the chosen region, then migrates to the local network and keeps its location.
- Proxy access for normal users and root, with SOCKS5 and HTTP CONNECT on loopback.
- Bidirectional dual-stack bridge access for root, with derived IPv4 `/30` and IPv6 `/126` subnets and host-controlled filtering.
- Ordered `--via` entries: `direct`, SOCKS5 relays, and filtered Mudfish nodes, expanded afresh for every bootstrap.
- `--location` constraints on per-family geo places, the edge colo, and per-family probe colos, rechecked after reconnects with automatic rebootstrap.
- `status` (with `--json`), `down`, and `warp-cli` commands for running instances.
- Foreground instances with systemd readiness notification.
- Zero Trust registration import without re-enrollment, automatic Team edge selection, and native TCP control-plane forwarding.
- Physical interface resolution per flow, following network changes.
- Structured logging configurable with `WAYWARP_LOG`.
- Nix flake for x86_64 and aarch64 Linux with the package, a NixOS module for named instances, a VM test, and a development shell that provides the runtime tools.
