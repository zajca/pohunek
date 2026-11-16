## Unreleased (2026-11-16, stored plugin index, #1614)



The daemon stores the official plugin index under `<plugins>/index/` (`official-v1.json` / `official-v2.json`) with a sidecar `stored.json` recording `stored_at` per schema. The CLI refreshes on demand only when the stored copy is older than 24 hours; there is no background refresh. `pohunek plugin index refresh` stages the bytes and calls `plugin.index.apply`, which verifies signature, expiry and rollback before storing and accumulating reserved runtime ids.

Doctor detail `plugin_index` reports `not_used` / `not_refreshed` / `current` / `stale` / `index_expired` / `index_invalid` / `package_revoked` (fail, names id and version) / `runtime_id_reserved_by_official` (warn). A revoked or delisted digest never blocks launch.

Installed packages keep their origin: local packages whose declared runtime id is now listed as reserved get `runtime_id_reserved_by_official` and still run. The namespace guard is data-driven: a local (`--sha256` or link) install is refused (`package_runtime_not_claimable`) when its declared runtime id is in the accumulated reserved set, so the Pi `--sha256` checkout flow works before the stored index lists `pi`. Each record carries a `provenance` (`index`/`catalog`/`local`); pre-change records upgrade by source (`Official` to `catalog`, the rest to `local`) and are never treated as index-origin.
