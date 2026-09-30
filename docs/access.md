# Bridge access and Zero Trust

## Routing

`up bridge` needs root. It creates a dual-stack host link, `waywarpINDEX`, with point-to-point IPv4 `/30` and IPv6 `/126` subnets. The namespace side is the gateway:

```sh
sudo waywarp up bridge
sudo ip route add 203.0.113.0/24 via 169.254.1.1 dev waywarp0
sudo ip -6 route add 2001:db8:1234::/48 via fd77:6179:7761:7270::1 dev waywarp0
```

Only the link subnets are added automatically, so nothing else uses WARP until you route it. Sockets bound to the link are the exception: a policy rule sends their traffic through WARP, as with `curl --interface waywarp0` or `ping -I waywarp0`. That rule has preference 32000 and looks up table `2002876160+INDEX`.

Network managers that remove foreign rules and routes, such as systemd-networkd by default, also remove these. Set `ManageForeignRoutingPolicyRules=no` and `ManageForeignRoutes=no` to keep them.

Each instance derives its own subnets from its index. `--subnet4` and `--subnet6` replace them if either overlaps one of your networks; `up` refuses subnets that overlap an existing host route.

The link takes the MTU of WARP's own link, so the host refuses oversized packets itself and tells their senders, and TCP MSS is clamped in both directions. Waywarp keeps the route, MTU, and NAT in step with WARP after every reconnect.

## Firewall and NAT

The private namespace permits both directions between the host link and WARP. Waywarp never adds host firewall rules or copies connector routes to the host: host routing and host nftables decide what reaches the link.

Outbound source NAT follows `--nat` (`access.bridge.nat` in NixOS):

| Mode | Behavior |
| --- | --- |
| `auto` (default) | Sources in the connector's advertised `routes` keep their addresses when `connector_config.nat_mode` is false; everything else is translated |
| `always` | Translate every source |
| `never` | Translate nothing; Cloudflare must route every source back to this node |

Translation always targets the device's assigned `interface.v4` or `interface.v6`, never a shared connector address. Consumer and ordinary Team registrations have no connector routes, so all their traffic is translated.

Unsolicited traffic for the assigned addresses is forwarded to the host side of the link. Other connector addresses stay in the namespace, so services such as Cloudflare mesh DNS keep working.

Waywarp follows the registration live: it watches `conf.json`, reconciles every five seconds and after reconnects, and replaces the NAT rules in one nftables transaction. It fails safe:

- A missing or malformed configuration removes routed exemptions, even with `never`, and keeps translating to previously verified addresses.
- An assigned address absent from WARP's current link blocks new traffic in that family rather than translating it to another address.

Existing connections keep their conntrack mapping, so route removals or address changes may require reconnecting them. `status` shows the mode, routed sources, and whether the configuration is valid; `status --json` also shows the verified targets. Registration contents are never logged.

## Zero Trust

Copy an existing Cloudflare Zero Trust registration into a root-owned instance without logging in again:

```sh
sudo waywarp import 3
warp-cli --accept-tos disconnect
sudo waywarp up bridge 3
warp-cli --accept-tos connect
```

`import` reads `/var/lib/cloudflare-warp` by default and leaves it unchanged; `--from` names another state directory and `--replace` overwrites an instance's registration. Waywarp recognizes Team registrations, selects their WARP edge, and forwards the direct TCP they need for policy and posture requests.

Disconnect the original client while the copy makes its first connection; reconnect it after `up` succeeds. Both can then stay connected, although an organization's policy may behave differently. Waywarp refreshes its own copy and never writes back to the original.

Zero Trust policy controls what access works. If it prohibits mode switching, as many include-only policies do, use bridge access. Enroll with the Cloudflare client first; Waywarp adds no separate enrollment flow.

Proxy access has no authentication, so any local user could use an imported identity through it. On shared machines, prefer bridge access for Zero Trust instances.
