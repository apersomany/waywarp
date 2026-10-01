# WARP routing observations

Though regional exits were not the reason I started Waywarp, while getting separate clients to work, I found that a WARP connection could keep its region after moving from a relay onto my own network.

This document records those observations and the routing model I use to interpret them. It is not a description of Cloudflare's documented internals. For choosing locations and reading `status`, see [location fields](locations.md).

## Moving off the relay

The useful behavior is straightforward: establish WARP through a relay in another region, then send its tunnel packets directly from the local network. The connection can retain its region even though the relay is no longer in the path.

Waywarp uses WARP's MASQUE protocol, which runs over QUIC. Its forwarding code switches the outer UDP flows from SOCKS5 associations to direct sockets. It does not need to decrypt the tunnel to do that.

Sometimes WARP reconnects while the new path settles rather than migrating the same session. Waywarp waits for that connection and checks the locations afterward. A direct reply alone is not enough to conclude that the requested region was retained.

## Connection IDs

In my experiments, reconnecting with the first 12 bytes of a connection ID retained could also preserve the region. That suggests Cloudflare uses information in the QUIC connection ID to route a session to its associated region, rather than choosing a new region solely from the packet's source address.

The 12-byte observation is not a public protocol guarantee. It does not establish the exact byte layout or mean arbitrary connection IDs can be reused indefinitely. Waywarp does not rewrite connection IDs; it changes the network path and checks the resulting connection.

I tried a similar approach when WARP was WireGuard-only, and it did not work then. Non-MASQUE QUIC traffic has also shown behavior consistent with connection-ID-based routing in my tests, though it is harder to inspect at the same level. These observations are not a promise that the same technique works for every QUIC service.

## A model for the locations

I find it useful to separate three parts of the path:

- **Tunnel ingress:** the Cloudflare-side endpoint associated with the WARP session, represented by `edge`.
- **Public source IP:** the address visible to a destination, with its advertised location represented by `geo4` or `geo6`.
- **Traffic egress:** where a particular request likely leaves Cloudflare's network, inferred for the test requests from `probe4` or `probe6`.

These are not necessarily in the same place. IPv4 and IPv6 can have different source IP locations and take different paths. All five location fields can differ.

BGP routing to Cloudflare's anycast addresses is part of how packets initially reach its network. It does not, by itself, explain the advertised location of the source IP or where every request exits. Connection-ID-based routing could explain how a session stays associated with a region after its source network changes. The reported `edge` should not be treated as a map of every intermediate hop.

Likewise, a trace response tells us which colo served that request, not the physical exit for every destination. Treating the probe colo as a likely egress location is useful for debugging, but it remains an inference. An IP geofeed is a separate statement about an address's advertised location.

## What Waywarp relies on

Waywarp relies on checking the result, not on this model being complete. After relay setup and migration, it reads the tunnel colo, makes IPv4/IPv6 probes, looks up their public IPs in the geofeed, and compares the fields with `--location`.

It repeats those checks after reconnects. If a required field changes, it tries the configured connection paths again unless `--no-rebootstrap` is set.

The relevant implementation is in [connection setup](../src/supervise/bootstrap.rs), [UDP migration](../src/dataplane/udp.rs), and [location probes](../src/location/probe.rs). For day-to-day use, the important distinction is still the same: asking for an IP advertised in Hong Kong is not the same as asking for an HKG tunnel endpoint or an HKG probe path.
