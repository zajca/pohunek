## Unreleased (2026-11-28, libghostty-vt becomes the default screen model, #2538 P3b)

- The default screen model is now libghostty-vt: `crates/terminal` builds with
  `default = ["libghostty"]`, and `crates/cli`, `crates/daemon`,
  `crates/session-worker` and `crates/xtask` forward it in their own
  `default`, so `TerminalTracker::new` runs libghostty in every default build.
  The case-table row that pins this (`new_reports_libghostty_in_its_snapshot_after_the_p3b_flip`)
  is live again.
- The vt100 product is the `--no-default-features` fallback build: no Zig on
  PATH, no ghostty crate in the worker tree, no C library linked. The new
  `terminal-vt100-fallback` lane of `ci.yml` proves it
  (`cargo test -p pohunek-terminal --no-default-features` plus a
  `cargo tree -p pohunek-session-worker --no-default-features -e normal`
  assertion) and must never carry the `libghostty-vt` action step; the guard
  test classifies it as `FALLBACK_LANES`.
- Every cargo lane of every workflow now provides the pinned library through
  the `libghostty-vt` / `libghostty-vt-macos` actions; `PENDING_LANES` in
  `scripts/tests/test_libghostty_vt.py` is empty. The macOS lanes download the
  cross-built `aarch64-apple-darwin` library as a run artifact from a
  `libghostty-darwin` producer job (`ci.yml`, `release-evidence.yml`, and one
  per row of `plugin-ci.yml`).
- No protocol bump: `screen_model`/`format_version` stay optional on all
  three wire layers (worker stays 9 with window 8..=9, public stays 5).
- Rollback: one line, `default = []` in `crates/terminal/Cargo.toml` (plus the
  guard-list move); the P3a versioning fields stay and are harmless.
- Source map: `crates/terminal/Cargo.toml` (+ the four consumer manifests),
  `scripts/tests/test_libghostty_vt.py`, `.github/workflows/{ci,gate-tests,plugin-ci,plugin-release,release-evidence,release}.yml`,
  `crates/terminal/tests/screen_backend.rs`, `docs/development.md`,
  `docs/architecture.md`, `docs/public-api.md`, `changelog.d/2538-p3b-default-flip.md`.
