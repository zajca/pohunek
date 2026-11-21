## Unreleased (2026-11-21, docs-check feature-table fact, #2484)

- `cargo xtask docs check` holds the runtime feature table of
  `guides/runtime-packages.md` to the committed registry snapshot
  (`crates/daemon/src/agent/host/fixtures/features.txt`): the `feature-table`
  fact fails on a missing, extra or differently dated feature id.
