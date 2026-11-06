## Unreleased (2026-11-06, plugin distribution follow-ups, #1967, #1969, #1971, #2023)

The [recovery runbook](runbooks/runtime-package-recovery.md) covers `plugin_trust_required`, `plugin_archive_digest_mismatch` and the unparsable package reference of `plugin_source_invalid`, and names `service_version_lacks_index_channel` beside `service_version_conflict`. The [key ceremony runbook](runbooks/plugin-index-keys.md) stages the offline primary seed as `$d/primary.key` and destroys it with `shred -u`. [Runtime catalog](concepts/runtime-catalog.md) lists `catalog keygen`.
