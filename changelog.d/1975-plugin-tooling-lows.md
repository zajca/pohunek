---
kind: changed
---
`cargo xtask index check-entry` takes its predicate names from one typed list, and the docs now list every name it can print. The refusal text is unchanged. `plugin-release.yml` installs the Rust toolchain of the core tag through the repository's `setup-rust` action instead of relying on implicit selection.
