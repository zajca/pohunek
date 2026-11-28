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
- The fallback is the whole product, not just the terminal crate: the root
  workspace dependency on `pohunek-session-worker` and plugin-kit's
  `pohunek-daemon` dependency now carry `default-features = false`, and the
  CLI/daemon forward `pohunek-session-worker/libghostty` in their `libghostty`
  feature, so `cargo build --workspace --no-default-features` (and per crate
  cli/daemon/session-worker) succeeds with `LIBGHOSTTY_VT_DIR` unset. The
  `terminal-vt100-fallback` lane proves it for terminal, session-worker, cli,
  daemon and the kit: the fallback CLI builds with
  `--no-default-features --features embed-agent-skill` (the embedded agent
  skill is not tied to the screen model, #2604 round 3) and the kit with
  `--no-default-features --features daemon` (its own `libghostty` feature,
  not hard-wired into `daemon`).
- Rollback is six manifest lines, not one (the consumers forward the feature
  in their own defaults): drop `"libghostty"` from `default` in
  `crates/terminal/Cargo.toml`, `crates/cli`, `crates/daemon`,
  `crates/session-worker`, `crates/xtask` and `crates/plugin-kit`. Proven by
  applying exactly those six lines: with `LIBGHOSTTY_VT_DIR` unset the trees
  of `pohunek-session-worker`, `pohunek-cli` and `pohunek-plugin-kit` (default
  features) hold no ghostty crate and `pohunek-sessiond` plus the kit build
  rc=0 with no Zig on PATH.
- #2602: `gate-tests.yml` (the release gate and the nightly) runs
  `--all-features`, so it now carries the `Provide libghostty-vt` step right
  after the checkout, before the first cargo command; the pin lives in
  `test_libghostty_vt.py`
  (`test_the_release_gate_provides_the_library_before_its_first_cargo_step`).
- Source map: `crates/terminal/Cargo.toml` (+ the four consumer manifests),
  `scripts/tests/test_libghostty_vt.py`, `.github/workflows/{ci,gate-tests,plugin-ci,plugin-release,release-evidence,release}.yml`,
  `crates/terminal/tests/screen_backend.rs`, `docs/development.md`,
  `docs/architecture.md`, `docs/public-api.md`, `changelog.d/2538-p3b-default-flip.md`.
