## Unreleased (2026-10-14, one-step upgrade to the official runtime packages, #1011)

- `pohunek service upgrade` moves a host on the built-in Claude Code and Codex
  runtimes to their official packages in one step, without
  `--accept-runtime-loss`: it installs the packages from the new release's
  `runtime/`, pins every host profile over them, adopts the live sessions,
  reinstalls the hooks, and leaves stopped sessions resumable, which relaunch
  from the package digest. `--accept-runtime-loss` now covers only a session the
  new daemon cannot adopt.
- The manual `plugin install` and `plugin profile migrate` steps remain the
  fallback for a host that upgrades without the runtime step.
- The upgrade E2E (`crates/cli/tests/release_upgrade.rs`) starts from the
  previous release's built-in profiles with a live and a stopped Claude session,
  runs the one-step upgrade, and asserts the pins, the adoption, the resume onto
  the package digest and new Claude and Codex sessions through the packages. It
  no longer takes the previous release's runtime directory
  (`POHUNEK_PREVIOUS_RUNTIME_DIR` is gone).
- `cargo xtask release test-runtime` writes a test-only runtime layout with a
  discarded key for the upgrade test; it is not a release input.
