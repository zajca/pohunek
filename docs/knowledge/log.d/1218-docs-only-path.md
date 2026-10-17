## Unreleased (2026-10-17, docs-only pull requests skip the build gates, #1218)

- A pull request whose every changed file is prose (no Rust tests and no gate
  but `docs check`) skips `fmt + clippy`, `darwin-check`, `doctests`,
  `build bins`, `SDK workspace`, `cargo-audit` and `cargo-shear`. A failed or
  skipped selection runs them all, and the `test selection verdict` job fails
  a run where one of them was skipped any other way. `build tests` and
  `real systemd supervision` still run on such a pull request.
