# Phase 1: Core Local Sessions

## Objective

Build the first functional `zagentmesh` daemon and CLI for durable local PTY
sessions. The daemon owns agent processes; the CLI starts, lists, inspects,
attaches, detaches, and stops sessions through a local Unix socket. Both Codex
and Claude Code run as PTY/TUI agents, with state detected from the terminal
stream and resume backed by native agent session IDs.

> Agent-integration details below are validated against the source of `herdr`
> (Rust + portable-pty + Tokio — the same stack) and `Kandev`. See
> [`plan-phase-1.md`](../plan-phase-1.md) for the concrete implementation plan.

## User Value

You can run Codex and Claude Code in real terminal sessions that survive client
disconnects, on your local machine, driven by a fast CLI with `--json` output —
usable for daily agent work before any remote capability exists.

## Scope

- Rust daemon owning PTYs and child processes (Tokio, `portable-pty`,
  thread-per-PTY blocking reader bridged to async).
- Rust `zagentmesh` CLI as the primary control surface.
- Local Unix socket control protocol: newline-delimited JSON requests, responses,
  and subscription events (Serde-typed envelopes), with `request_id` and protocol
  version.
- Separate raw-byte attach connection per PTY (see architecture: Attach
  Streaming).
- Session lifecycle: new, list, inspect, attach, detach, stop, status.
- Run Codex and Claude Code in PTY/TUI mode (both first-class) behind a small
  per-agent adapter (launch command, input-injection rules, state manifest,
  resume command).
- Agent state from the terminal stream: OSC title/progress (primary), screen-
  content pattern matching via per-agent TOML manifests (fallback), and PTY
  activity for the working signal, debounced behind a stability window. Process
  state + exit code for done/failed. State records its `source`. **Codex and
  Claude Code do NOT report live state via hooks.**
- A virtual terminal emulator over each PTY to extract the visible screen for
  detection and incremental OSC parsing.
- Native agent session ID captured via a `SessionStart` hook for resume.
- Worktree-per-session isolation with ownership checks.
- SQLite metadata (`state.db`) with schema versioning.
- Structured logs and a local append-only event log for session lifecycle.
- Owner-private socket (`0700` dir, `0600` socket).

## Out of Scope

- Remote hosts / NetBird transport (Phase 2).
- NetBird discovery (Phase 2).
- Provider integrations (deferred; later doc).
- Native GUI (deferred; later doc).
- ACP runtime (deferred; PTY/TUI-first).
- Multi-user authorization.

## Deliverables

- Daemon binary: local PTY ownership and session supervision, runnable as a
  systemd user service, with single-instance lock and stale-socket recovery.
- CLI commands:
  ```bash
  zagentmesh doctor
  zagentmesh daemon start
  zagentmesh session new --agent <codex|claude> [--repo <path>] [--branch <name>]
  zagentmesh session list
  zagentmesh session inspect <session-id>
  zagentmesh attach <session-id>
  zagentmesh session stop <session-id>
  zagentmesh status
  ```
- Control protocol implementation (newline-JSON) with typed envelopes and version
  negotiation.
- Separate attach-stream connection with resize/detach over the control
  connection.
- PTY-backed session lifecycle for shell, Codex, and Claude Code.
- Per-agent adapter for Codex and Claude Code:
  - launch command;
  - **PTY input-injection rules** — Claude Code (Ink TUI) needs bracketed paste
    disabled and the submit byte (`\r`) sent as a separate write after a ~150 ms
    delay; other agents wrap multi-line prompts in bracketed paste
    (`ESC[200~`…`ESC[201~`) and submit with `\r`;
  - state-detection manifest (TOML: OSC rules + screen-region rules with
    contains/regex/any/not gates);
  - `SessionStart` hook (fire-and-forget over the socket) that posts the native
    session ID / transcript path;
  - resume command (`claude --resume <id>`, `codex resume <id>`).
- State engine: incremental OSC parser, virtual-terminal screen extraction,
  manifest matcher, and a debounced state machine (stability window).
- Worktree creation/binding/ownership and safe reuse.
- SQLite persistence: sessions, agent type, working directory, branch/worktree,
  PTY size, lifecycle state + source, timestamps, exit status, native resume IDs.
- Structured logging and event-log records.
- `--json` output for `list`, `inspect`, `status`.

## Architecture Impact

Phase 1 establishes the daemon/CLI boundary and the protocol that Phase 2 reuses
verbatim over NetBird. The daemon owns OS PTYs and processes; the CLI is a thin
controller and attach client. The control protocol and the separate attach-stream
model are designed once here so the remote phase needs no second protocol. The
per-agent adapter boundary is established here and exercised immediately by two
agents (Codex + Claude Code), so adding agents later is data + a small adapter,
not core changes.

## CLI/UX Implications

- Default compact tables for humans; `--json` for agents and automation.
- Errors state the failed operation, likely cause, and the next recovery command.
- Detach must make clear the daemon-owned session keeps running.
- Every command is agent-operable: stable `--json` plus a subscription/await
  capability for state changes is the operator-agent foundation.

## Data/Protocol Implications

- One JSON value per line for control; one response per ordinary request;
  subscriptions stream event envelopes.
- Events: session created/updated/stopped, attach opened/closed, agent state
  changed (with `source`), daemon error.
- Attach byte streaming uses the separate connection; control stays newline-JSON.
- Session records never store environment secrets.
- The `SessionStart` hook submits a small RPC carrying `{session_id,
  transcript_path?, agent}`; the daemon stores it as the resume binding.

## Testing and Verification

- Unit-test protocol serialization for request/response/error/event + version
  negotiation.
- Unit-test session state transitions (start, attach, detach, stop, exit, fail).
- Integration-test daemon startup, single-instance lock, stale-socket recovery.
- Integration-test create/list/inspect a local PTY session via CLI.
- Integration-test attach, detach, reattach without killing the process.
- Integration-test the separate attach stream: arbitrary binary output survives;
  resize/detach over control work while attached.
- Test the state engine with recorded terminal fixtures per agent: OSC spinner →
  working, idle title → idle, Codex "Action Required" → blocked, Claude approval
  form → blocked; debounce prevents flicker; OSC fragmentation across reads.
- Test input injection: Claude bracketed-paste-off + delayed submit actually
  submits; bracketed-paste multi-line prompt for other agents.
- Test native resume ID capture and resume command for Codex and Claude Code
  where installed.
- Verify `--json` output is machine-parseable for list/inspect/status.
- Verify missing Codex/Claude Code binaries produce clear diagnostics.
- Verify SQLite schema load + a forward migration.

## Success Criteria

- Start the local daemon and confirm health via the CLI.
- Create a local PTY session, attach, detach, reattach without stopping the
  process.
- Run both Codex and Claude Code as PTY/TUI agents under daemon ownership, with
  prompts injected correctly (incl. Claude's Ink submit quirk).
- Agent state visible via CLI and `--json`, sourced from OSC titles + screen
  detection, with `source` shown.
- Control protocol uses newline-JSON; attach uses a separate raw connection.
- Session metadata, event log, and structured logs persist across CLI exits;
  sessions resume via captured native session IDs after a daemon restart.
- Worktree-per-session prevents accidental shared working trees.
- No remote, discovery, provider, GUI, or central dependency required.

## Risks

- `blocked`/awaiting-approval is the trickiest state — detected via OSC title
  (Codex "Action Required") and screen-content prompt patterns; validate the
  manifest rules empirically per agent and per agent-CLI version.
- Agent CLIs change under us (TUI layout, flags). Mitigation: keep detection in
  TOML manifests and pin behavior with recorded fixtures; keep adapters thin.
- Daemon restart cannot preserve live PTYs; rely on native resume IDs and
  document the limit.
- `portable-pty` is blocking; bridge to async via a reader thread + channel.
- OSC sequences fragment across reads; the parser must be stateful and clear
  evidence on foreground-process change.
- CLI grammar chosen here must accommodate Phase 2 host targeting
  (`host/session-id`); design target syntax now.

## Exit Criteria

- Durable local session workflows usable through the CLI.
- The control protocol and attach-stream model are documented well enough for
  Phase 2 to reuse them over NetBird unchanged.
- Tests cover the local lifecycle, protocol, attach stream, state engine, input
  injection, JSON output, logging, and a schema migration.
- Known limits (daemon-restart resume, `blocked` detection) are explicit and do
  not block Phase 2.
