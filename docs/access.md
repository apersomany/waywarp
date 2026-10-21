# Bridge access and Zero Trust

Bridge access gives the host a network link into a WARP instance. It needs root, but does not make WARP the host's default connection.

## Routing

`up bridge` creates `waywarpINDEX`, with an IPv4 `/30` and IPv6 `/126` subnet. The namespace end is the gateway. New registrations require [terms acceptance](instances.md#names-and-lifecycle). For index `0`:

```sh
sudo waywarp up bridge --accept-tos
sudo ip route add 203.0.113.0/24 via 169.254.1.1 dev waywarp0
sudo ip -6 route add 2001:db8:1234::/48 via fd77:6179:7761:7270::1 dev waywarp0
```

Replace these example destinations with your networks. Waywarp adds only the link subnets; other traffic uses WARP when you add a route or bind a socket to the link:

```sh
sudo curl --interface waywarp0 https://www.cloudflare.com/cdn-cgi/trace
sudo ping -I waywarp0 1.1.1.1
```

Bound sockets use a policy rule with preference `32000`, which looks up table `2002876160 + INDEX`.

Network managers can remove rules and routes they did not create. systemd-networkd does this by default. Set `ManageForeignRoutingPolicyRules=no` and `ManageForeignRoutes=no` if you want it to leave Waywarp's rules and routes alone.

Each index gets different default subnets. Override them with `--subnet4` and `--subnet6` if needed; they must be canonical `/30` and `/126` networks and must not overlap existing host routes.

The bridge follows WARP's current routes, MTU, and NAT after reconnects and link/address changes. Its MTU lets the host reject oversized packets, and TCP MSS is clamped in both directions.

## Firewall and NAT

Namespace-local rules allow traffic in both directions between the host link and WARP. Waywarp adds no host firewall rules or connector routes on the host; your host routes and nftables rules decide what reaches the link.

Choose outbound source NAT with `--nat`, or `access.bridge.nat` in NixOS:

| Mode | Behavior |
| --- | --- |
| `auto` (default) | Keep sources in the connector's advertised `routes` unchanged when `connector_config.nat_mode` is false; translate everything else |
| `always` | Translate every source |
| `never` | Leave sources unchanged; Cloudflare must know how to route them back to this node |

When translating, Waywarp uses the device's assigned `interface.v4` or `interface.v6`, not a shared connector address. Consumer and ordinary Team registrations have no connector routes, so `auto` translates all of their outgoing traffic.

Incoming traffic for those assigned WARP addresses is forwarded to the host side of the link. Other connector addresses stay inside the namespace. That keeps services such as Cloudflare mesh DNS local to the connector.

### Registration changes

NAT policy follows changes to `conf.json` and WARP's link addresses. Missing or malformed configuration removes routed-source exceptions and falls back to translation using the last verified targets, overriding `never` until the configuration is valid.

A translation target must still be present on WARP's current link. Traffic that needs translation is blocked for an address family with no valid target, rather than being translated to some other address. This does not block sources already exempt from translation, or the `never` path when the configuration is valid.

Existing connections keep their conntrack mappings. If a route or assigned address changes, you may need to reconnect them before the new policy takes effect.

`status` reports NAT mode, routed sources, verified translation targets, and configuration warnings. The same information is available in `status --json`. Registration contents are never logged.

## Zero Trust

You can copy an existing Cloudflare Zero Trust registration into a root-owned instance without logging in again:

```sh
sudo waywarp import 3
warp-cli --accept-tos disconnect
sudo waywarp up bridge 3
warp-cli --accept-tos connect
```

Keep the original disconnected until the copy's first `up` succeeds, then reconnect it. Both can stay connected, subject to your organization's policy.

`import` reads `/var/lib/cloudflare-warp` by default. Use `--from` for another state directory or `--replace` to overwrite an instance's existing registration. Waywarp refreshes only its own copy; the source is never changed.

Team endpoints and required direct TCP requests for policy and posture checks are handled automatically.

### Policy and local access

Your organization's policy still controls what the instance can reach and which modes it can use. If it disallows switching to proxy mode, as many include-only policies do, use bridge access. Enroll through the Cloudflare client first; Waywarp does not provide a separate enrollment flow.

**Proxy access has no authentication.** Any local user could use an imported identity through its proxy. On shared machines, prefer bridge access for Zero Trust instances.
