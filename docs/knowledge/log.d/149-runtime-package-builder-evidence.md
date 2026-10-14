## Unreleased (2026-10-12, runtime package builder evidence, #149)

- `cargo xtask package build --evidence-dir <D>` writes the builder evidence of
  an official package next to its archive: `<archive>.sha256`,
  `<rt>.manifest.json`, `<rt>.provenance-input.json` (source commit and Git
  tree, builder, archive entries, the lock's whole `upstream` object of any
  kind) and `<rt>.compat-vector.json` (the package half of a compatibility
  attestation),
  as deterministic JSON. A package tree, lock or matrix that differs from
  `HEAD`, a missing or mismatched lock and an evidence directory inside the
  package are refused with nothing written. The per-PR package jobs upload it
  as `package-evidence-<rt>`, the release build as `package-evidence`; it is
  not a release asset. Documented in `concepts/runtime-package-archive` and
  `concepts/release-bundle`.
