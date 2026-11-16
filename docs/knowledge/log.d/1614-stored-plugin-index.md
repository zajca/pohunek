## Unreleased (2026-12-01, stored plugin index, #1614)



The daemon stores the official plugin index under `<plugins>/index/` (`official-v1.json` / `official-v2.json`) with a sidecar `stored.json` recording `stored_at` per schema. The CLI refreshes on demand only when the stored copy is older than 24 hours; there is no background refresh. `pohunek plugin index refresh` stages the bytes and calls `plugin.index.apply`, which verifies signature, expiry and rollback before storing and accumulating reserved runtime ids.

Doctor detail `plugin_index` reports `not_used` / `not_refreshed` / `current` / `stale` / `index_expired` / `index_invalid` / `package_revoked` (fail, names id and version) / `runtime_id_reserved_by_official` (warn). A revoked or delisted digest never blocks launch.

Installed packages keep their origin: local packages whose digest is now listed as reserved get `runtime_id_reserved_by_official` and still run. Namespace guard is data-driven: a local `pohunek.runtime.*` package is refused (`package_namespace_reserved`) only when the verified stored index lists that package id, so the Pi `--sha256` checkout flow works before admission.
