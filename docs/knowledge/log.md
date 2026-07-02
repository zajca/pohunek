# Knowledge Bundle Log

## 0.7.4

- Documented that the native GUI `Start session` runtime picker supports the
  built-in `shell`, `codex`, and `claude` choices.

## 0.7.3

- Documented that `pohunek-gui` is Wayland-only and fails fast when
  `WAYLAND_DISPLAY` is missing or empty.

## 0.7.2

- Documented explicit `session.resume` and the GUI "Open in terminal" behavior
  for terminal sessions with native resume metadata.

## 0.7.1

- Documented that release archives include the native `pohunek-gui` binary and
  linked release packaging files from the assistant source map.

## 0.7.0

- Added native GUI provider-integration guidance for Linear and GitHub, including
  `gui.toml` provider configuration, keyring and `gh` boundaries, request
  staleness handling, provider launch flow, `link.*` metadata, and PR status
  degradation rules.
- Linked the new GUI provider implementation and provider test files from the
  assistant source map.

## 0.5.0

- Added GUI setup guidance for `pohunek-gui`, including `gui.toml`, external
  attach delegation, Wayland startup diagnostics, project/worktree method
  boundaries, and secret-handling rules.
- Added native GUI prompt-management guidance covering host-resolved
  prompt/action browse, preview rendering through `crates/prompt`, and
  `session.new input` launch behavior.
- Linked GUI setup from the knowledge index and source map.

## 0.3.3

- Added the Phase 1 hand-authored knowledge skeleton for the Universal Pohunek
  Assistant.
- Reserved `index.md` for navigation and `log.md` for bundle history.
- Kept generated reference content out of the committed tree.
