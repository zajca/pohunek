---
kind: added
---
The package crate persists the per-host plugin index state (`index-state.json`: index and revocation high-water marks plus the accumulated revoked key ids, merged only upward under the registry lock) and parses `plugin-index-channel.json`, the release file that names the index endorser root of the trust anchor. Nothing reads them yet; the daemon and CLI wiring follow. ([#1930](https://github.com/zajca/pohunek/issues/1930))
