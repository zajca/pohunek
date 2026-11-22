## Unreleased (2026-11-22, assistant installs its missing package, #2511)

- `pohunek assistant` recovers from the daemon's `assistant_package_missing`
  refusal instead of printing the old placeholder install command. A local
  launch runs the shared `setup_assistant` plan against the daemon's core
  version: an `Install` step previews the version and digest, asks once
  (`--yes` skips the question; with `--json` or without a TTY and without
  `--yes` it prints the preview and stops), installs through the daemon's
  signed plugin index verification, and retries the launch once. A `Gap` or
  `Present` step prints the plan's outcome with the error, and the returned
  error carries that outcome as its recovery hint, so the stale daemon remedy
  never reaches the output. A remote launch prints
  `pohunek assistant setup` for that host and installs nothing remotely,
  because `plugin install` is local-only. When the official index lists no
  assistant package the daemon's core can use (the id is absent or no entry
  supports that core), the message names the core version and says to rerun
  `pohunek assistant setup` after admission; a platform-only gap keeps the
  resolver's message and never claims a core reason. The daemon's own refusal
  names `pohunek assistant setup` (or `pohunek setup`) with no core claim and
  no `<placeholder>`, as do the not-selected, disabled and rejected arms. The
  match keys on the stable error code alone, never on the message or remedy,
  so an older daemon's placeholder hint cannot leak through. Covered by
  `crates/cli/tests/assistant_setup_install.rs` (S1-S6).
