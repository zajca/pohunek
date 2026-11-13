---
kind: added
---
The owner can list static NetBird peers in `overlays.toml` (`[netbird.static_peers.<name>]` with `address`, `fqdn` and a required `host_id` under `[netbird] subnet`), so a non-owner account under NetBird 0.83+ reaches the configured peer. The client verifies the pinned host id with `host.governance.inspect` before any other request; a mismatch is `host_identity_mismatch`. Static peers never appear in `host discover`/`host list`. An older release rejects the new table as an unknown key. ([#2310](https://github.com/zajca/pohunek/issues/2310))
