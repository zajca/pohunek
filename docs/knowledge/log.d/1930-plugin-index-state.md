## Unreleased (2026-11-05, plugin index state and channel parser, #1930)

- `crates/package/src/index_state.rs` adds `Registry::index_state` and `Registry::record_index`; `crates/package/src/index_channel.rs` adds `parse_index_channel`; see [Plugin index](concepts/runtime-catalog.md#plugin-index) and [Index state](concepts/runtime-package-archive.md#index-state). No daemon, CLI or protocol surface changes.
