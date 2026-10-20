## Unreleased (2026-10-20, release runs the docs check on the folded tree, #1191)

`scripts/release` runs `cargo xtask docs check`, which includes the
knowledge-log check, on the folded and bumped tree before it commits or pushes
the release commit. A failure stops the release, restores the tree and leaves
nothing pushed. `scripts/release --dry-run` folds and bumps a temporary export
of `HEAD` and runs the same check, so a fold that removes a path a page cites,
such as `docs/knowledge/log.d`, shows up before the release.
