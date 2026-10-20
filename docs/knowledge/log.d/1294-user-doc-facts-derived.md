## Unreleased (2026-10-20, hand-maintained user doc facts checked, #1294)

`cargo xtask docs check` compares the crate table of `docs/development.md` with
the workspace members (`crate-table`), the package list and any stated package
count of `README.md`, `docs/install.md` and `docs/features.md` with
`runtime-packages/` (`package-lists`), and requires every `docs/*.md` page and
knowledge page to be reachable from `docs/README.md` or
[the index](index.md) (`doc-index`). The Apple Silicon daemon archive carries
the runtime catalog and the four official packages, as the Linux daemon
archives do.
