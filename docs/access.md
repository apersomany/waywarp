# Bridge access and Zero Trust

Bridge access gives the host a network link into a WARP instance. It needs root, but does not make WARP the host's default connection.

## Routing

For a new registration, review [Cloudflare's Terms of Service](https://www.cloudflare.com/application/terms/) and pass `--accept-tos` to agree. Saved and imported registrations do not need the flag.

`up bridge` creates `waywarpINDEX`, with an IPv4 `/30` and IPv6 `/126` subnet. The namespace end of the link is the gateway. For index `0`, you can add routes like these:

```sh
sudo waywarp up bridge --accept-tos
sudo ip route add 203.0.113.0/24 via 169.254.1.1 dev waywarp0
sudo ip -6 route add 2001:db8:1234::/48 via fd77:6179:7761:7270::1 dev waywarp0
```

The destinations above are examples; replace them with the networks you want to reach through WARP.

Waywarp adds the link subnets, but no routes for other destinations. Traffic uses WARP when you add a route or bind a socket to the link:

```sh
sudo curl --interface waywarp0 https://www.cloudflare.com/cdn-cgi/trace
sudo ping -I waywarp0 1.1.1.1
```

Bound sockets use a policy rule with preference `32000`, which looks up table `2002876160 + INDEX`.

Network managers can remove rules and routes they did not create. systemd-networkd does this by default. Set `ManageForeignRoutingPolicyRules=no` and `ManageForeignRoutes=no` if you want it to leave Waywarp's rules and routes alone.

Each index gets different default subnets. If they conflict with your network, choose others with `--subnet4` and `--subnet6`. IPv4 subnets must be canonical `/30` networks and IPv6 subnets canonical `/126` networks. `up` rejects subnets that overlap an existing host route.

The bridge uses WARP's MTU, so the host can reject oversized packets and tell their senders. Waywarp also clamps TCP MSS in both directions. Routes, MTU, and NAT are updated after reconnects so they follow WARP's recreated link.

## Firewall and NAT

Inside the private namespace, traffic is allowed in both directions between the host link and WARP. Waywarp adds no host firewall rules and does not copy connector routes to the host. Your host routes and nftables rules decide what reaches the link.

Choose outbound source NAT with `--nat`, or `access.bridge.nat` in NixOS:

| Mode | Behavior |
| --- | --- |
| `auto` (default) | Keep sources in the connector's advertised `routes` unchanged when `connector_config.nat_mode` is false; translate everything else |
| `always` | Translate every source |
| `never` | Leave sources unchanged; Cloudflare must know how to route them back to this node |

When translating, Waywarp uses the device's assigned `interface.v4` or `interface.v6`, not a shared connector address. Consumer and ordinary Team registrations have no connector routes, so `auto` translates all of their outgoing traffic.

Incoming traffic for those assigned WARP addresses is forwarded to the host side of the link. Other connector addresses stay inside the namespace. That keeps services such as Cloudflare mesh DNS local to the connector.

### Registration changes

Waywarp watches `conf.json` and checks the bridge every five seconds and after reconnects. Changed NAT rules are applied in one nftables transaction.

If the configuration is missing or malformed, Waywarp removes the routed-source exceptions and falls back to translation using the last verified targets. This also overrides `never` while the configuration is invalid.

A translation target must still be present on WARP's current link. Traffic that needs translation is blocked for an address family with no valid target, rather than being translated to some other address. This does not block sources already exempt from translation, or the `never` path when the configuration is valid.

Existing connections keep their conntrack mappings. If a route or assigned address changes, you may need to reconnect them before the new policy takes effect.

`status` shows the NAT mode, routed sources, and whether the configuration is valid. `status --json` also shows the verified translation targets. Registration contents are never logged.

## Zero Trust

You can copy an existing Cloudflare Zero Trust registration into a root-owned instance without logging in again:

```sh
sudo waywarp import 3
warp-cli --accept-tos disconnect
sudo waywarp up bridge 3
warp-cli --accept-tos connect
```

Disconnect the original client while the copy makes its first connection. Once `up` succeeds, reconnect the original. Both can then stay connected, although your organization's policy may impose other restrictions.

`import` reads `/var/lib/cloudflare-warp` by default and leaves the source unchanged. Use `--from` for another state directory, or `--replace` to overwrite the instance's existing registration. Waywarp refreshes its own copy and never writes changes back to the original.

Waywarp recognizes Team registrations, chooses their WARP edge, and forwards the direct TCP requests needed for policy and posture checks.

### Policy and local access

Your organization's policy still controls what the instance can reach and which modes it can use. If it disallows switching to proxy mode, as many include-only policies do, use bridge access. Enroll through the Cloudflare client first; Waywarp does not provide a separate enrollment flow.

**Proxy access has no authentication.** Any local user could use an imported identity through its proxy. On shared machines, prefer bridge access for Zero Trust instances.
