## Unreleased (2026-11-05, signed plugin index verifier, #1929)

- `crates/package/src/plugin_index.rs` adds the pure `verify_index`, `parse_index_envelope` and `VerifiedIndex::authorize`; see [Plugin index](concepts/runtime-catalog.md#plugin-index). No daemon, CLI or protocol surface changes.
