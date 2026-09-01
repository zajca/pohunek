# Knowledge Bundle Log

## Unreleased (2026-08-12)

- Documented the simplified session-first native GUI: prioritized cross-host
  groups, modal session detail, direct lifecycle actions, and removal of the
  Agents monitor, provider browsers, review, and worktree-management surfaces.
- Documented standard Tab/Shift+Tab form focus, Enter/Ctrl+Enter submission,
  modal focus containment, and mouse selection/copy for native GUI detail text.
- Documented source-locked, credential-free Hermes compatibility CI and
  extracted release-archive plugin smoke verification.
- Documented configurable Hermes plugin timeout, output, screen, and concurrency
  bounds plus protocol-range repair during integration update.
- Added the Hermes operator guide: explicit managed runtime selection, isolated
  plugin lifecycle, access and host policy, complete typed tool surface,
  origin-session protection, bounded control-loop recovery, and payload-free
  lifecycle reporting.
- Clarified that native GUI releases target glibc x86_64 because Wayland and
  graphics libraries remain dynamic runtime dependencies; MUSL archives remain
  available for the CLI and daemon.
- Documented durable per-session PTY workers, daemon restart reconciliation,
  runtime state and events, explicit native recovery, systemd diagnostics, and
  the one-time legacy migration boundary.
- Documented browser session lifecycle and metadata management, the host-scoped
  Projects screen, and daemon-authoritative worktree removal safeguards.
- Documented the M1 web control center, browser-safe TypeScript SDK entry,
  backend deployment boundary, and fixture-backed development stack.
- Documented the self-contained Linux x86_64 web release archive and its
  systemd user-service installation flow.
- Documented that the native GUI `Start session` and Review "Dispatch as
  session…" agent pickers use the selected host's launchable runtime inventory,
  including resolvable profiles. The GUI fails closed when that inventory is
  unavailable instead of deriving launch permission from the name-only
  `supported_agents` field or falling back to built-in base kinds.
- Cross-linked agent profiles from the GUI Session Launch section.
- Documented fail-closed daemon shutdown when an overlay listener supervisor
  exits or panics unexpectedly after readiness.

## 0.18.3

- Documented native GUI keyboard list navigation and configurable
  `[keybindings]` overrides in `gui.toml`, including default binding names,
  key-string syntax, and same-context conflict validation.
- Linked GUI keyboard/config/reducer/provider source files from the assistant
  source map.

## 0.15.3

- Documented session notification dedupe/debounce behavior for
  `turn:<session_id>`: turn-completed creates share the debounce window,
  resolve on session resume, supersede older unread turn rows, and are collapsed
  by visible attention records.

## 0.15.2

- Documented the attention notification debounce window
  (`attention_debounce_secs`), the deferred-create behavior for
  `agent_blocked`/`approval_required` (`created: true` with a minted id, held
  pending until it flushes or is suppressed by an in-window resolve), and its
  relationship to `attention_dedupe_window_secs` in the sessions concept, the
  debug-daemon runbook, and the public API reference.

## 0.14.5

- Documented the durable cross-host notification Inbox, CLI/API surface,
  provider hook requirements, source-priority dedupe, retention, and trust-model
  boundary.

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
