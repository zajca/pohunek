---
kind: fixed
---
An account that does not own the NetBird 0.83 connection now gets a `netbird_state_restricted` hint that names the `[netbird.static_peers.<name>]` table and the host-id command, or `sudo netbird profile claim`. A failure of several overlays keeps that hint, and the message says whether the host is not listed or the NetBird daemon is not Connected. Holding NetBird below 0.83 is no longer suggested. ([#2382](https://github.com/zajca/pohunek/issues/2382))
