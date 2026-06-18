# Phase 1 Implementation Plan: Core Local Sessions

This is the concrete build plan for [Phase 1](phases/01-core-local-sessions.md).
It is grounded in the source of two reference projects analyzed for this design:

- **herdr** — Rust + `portable-pty` + Tokio + TOML detection manifests + Unix
  socket RPC. The closest reference (same stack). Paths below are
  `herdr/src/...` from commit `d998753` (2026-06-16).
- **Kandev** — Go + TypeScript. Used for the agent-adapter boundary, ACP vs PTY
  normalization, PTY input-injection quirks, and the worktree model. Paths below
  are `kandev/apps/backend/...` from commit `f9608ba` (2026-06-16).

Where these informed a decision, the file is cited so the implementer can read
the original.

## Definition of Done

- `zagentmesh daemon start` runs as a systemd user service; `zagentmesh doctor`
  reports health.
- `zagentmesh session new --agent {codex|claude}` starts the agent in a
  daemon-owned PTY; `attach`/detach/reattach work without killing it, and a
  reattaching client is replayed the recent screen + scrollback.
- Agent state (`working` / `blocked` / `idle` / `done` / `failed`) is visible in
  `status` and `--json`, sourced from OSC titles + screen detection, with the
  `source` shown.
- Prompts inject correctly, including Claude Code's Ink submit quirk.
- After a daemon restart, sessions resume via captured native session IDs.
- Worktree-per-session isolation works with ownership checks.
- Tests cover protocol, attach stream, the state engine (fixtures per agent),
  input injection, resume, JSON output, and a schema migration.

## Tech Stack and Crates

| Concern | Crate | Notes |
|---|---|---|
| Async runtime | `tokio` (full) | de-facto standard; daemon is async |
| PTY ownership | `portable-pty` | what herdr uses; blocking reader thread bridged to async |
| Newline-JSON framing | `tokio-util` `LinesCodec` + `serde_json` | one JSON value per line for control |
| Serialization | `serde`, `serde_json` | typed envelopes |
| VT screen extraction | `vt100` (or `alacritty_terminal`) | parse PTY output into a screen grid for detection; pure-Rust, low risk for Phase 1 |
| OSC parsing | custom incremental parser (see §6) | OSC 0/2/9; sequences fragment across reads |
| Detection manifests | `toml`, `regex` | herdr-style rule files |
| Embedded store | `rusqlite` (bundled SQLite) | single local DB; simplest for a daemon |
| Schema migration | `rusqlite_migration` or hand-rolled `user_version` | versioned from day one |
| CLI | `clap` (derive) | subcommands + `--json` |
| Logging | `tracing` + `tracing-subscriber` (JSON) | structured logs to state dir |
| Errors | `thiserror` (typed) + `anyhow` at edges | typed error classes for `--json` |

> The GUI (deferred) will use `libghostty-vt`. For Phase 1 we deliberately use a
> pure-Rust VT crate (no Zig toolchain) for screen extraction. Re-evaluate
> sharing `libghostty-vt` for both when the GUI work starts.

## Cargo Workspace Layout

```text
zagentmesh/
  Cargo.toml                 # workspace
  crates/
    protocol/                # shared: envelopes, request/response/event types, versioning
    daemon/                  # the host daemon (bin: zagentmeshd)
      src/
        pty/                 # PTY actor (thread-per-PTY, resize, reader bridge)
        session/             # session model + supervisor
        agent/               # agent adapter trait + codex.rs + claude.rs + manifests/*.toml
        detect/              # OSC parser, VT screen extraction, manifest matcher, state machine
        store/               # SQLite + migrations
        api/                 # Unix socket server, control protocol handler, attach stream
        events/              # append-only event log
    cli/                     # zagentmesh CLI (bin: zagentmesh)
```

Rationale: `protocol` is shared so the CLI and daemon cannot drift, and Phase 2's
NetBird transport reuses it unchanged.

## Control Protocol (Local Unix Socket)

Newline-delimited JSON. One JSON value per line. Socket at
`$XDG_RUNTIME_DIR/zagentmesh/daemon.sock` (`0700` dir, `0600` socket).

Envelope sketch (illustrative):

```jsonc
// request
{"v":1,"id":"req-7f3","method":"session.new","params":{"agent":"claude","repo":"/p","branch":"feat/x"}}
// response (ok)
{"v":1,"id":"req-7f3","ok":{"session_id":"s-42","state":"working"}}
// response (typed error)
{"v":1,"id":"req-7f3","err":{"class":"runtime","code":"agent_binary_missing","msg":"...","recover":"install claude"}}
// event (on a subscription connection)
{"v":1,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title","ts":"..."}
```

- `v` = protocol version. New fields are additive; unknown fields ignored. On
  connect, client and daemon exchange `v`; an incompatible pair fails with a
  typed `daemon/version_mismatch` error (not undefined behavior).
- `id` correlates request/response and related events (carried into Phase 2
  cross-host logs).
- Methods (Phase 1): `daemon.health`, `session.new`, `session.list`,
  `session.inspect`, `session.stop`, `session.attach`, `session.detach`,
  `session.resize`, `status`, `subscribe`.
- Reference: herdr's line-delimited JSON RPC (`herdr/src/api/server.rs`,
  `herdr/src/protocol/wire.rs`) and its hook RPC `pane.report_agent_session`.

## Attach Stream (Separate Connection)

Raw terminal bytes never share the JSON control connection.

1. Control: client sends `session.attach {session_id}` → daemon returns
   `{stream_id}`.
2. Client opens a **second** Unix-socket connection, sends a one-line header
   `{"attach":"<stream_id>"}\n`, after which the connection is a raw bidirectional
   byte pipe: PTY output down, keystrokes up.
3. `session.resize {session_id, cols, rows}` and `session.detach {stream_id}` go
   on the **control** connection while attached.
4. Multiple clients may attach; last-attach-wins resize, explicit resize
   available.
5. **History replay on attach.** Each session keeps a bounded ring buffer of
   recent raw PTY output (cap configurable, default 10 MB, in memory only). On
   attach the daemon writes that buffer to the new client *before* live bytes,
   so attaching to an idle session shows the current screen + scrollback instead
   of a blank terminal. The snapshot and the live subscription are taken
   atomically (shared mutex with the PTY reader) so the client sees every byte
   exactly once — no gap, no duplicate. Raw byte-exact replay; refinement to
   skip/trim replay in alternate-screen (TUI) mode like herdr is deferred to the
   agent-adapter milestone.

Reference: herdr keeps PTY ownership in the daemon and treats the pane as the
attach unit (`herdr/src/server/terminal_attach.rs`); it bounds scrollback by
bytes (`DEFAULT_SCROLLBACK_LIMIT_BYTES = 10 MB`) and skips replay in the
alternate screen (`herdr/src/pane.rs` `handoff_history_ansi`). Detach does not
kill the PTY.

## PTY Ownership (Actor Model)

Follow herdr's actor-per-PTY (`herdr/src/pty/actor.rs`, `actor/unix.rs`):

- One dedicated OS thread per PTY does blocking reads in 8 KB chunks
  (`portable-pty` master is blocking). Bridge to async via a channel
  (`tokio::sync::mpsc`) or `spawn_blocking`.
- Each read chunk is (a) forwarded to attached clients (broadcast), (b) fed to
  the VT emulator + OSC parser for detection, (c) appended to a bounded
  per-session output ring buffer (cap configurable, default 10 MB) used to
  replay recent screen + scrollback to a newly attached client. The push and the
  broadcast send share one mutex so attach can snapshot+subscribe atomically.
- `Handle`: `write_user_input(bytes)`, `resize(cols, rows, cell_px)`, `shutdown`.
- Resize via `master.resize(PtySize{...})`; queue resize separately from data
  writes (herdr injects terminal responses after resize completes).

## Agent Adapter Boundary

A small trait per agent (Kandev's `Agent` interface is the model —
`kandev/.../agent/agents/agent.go:26-64`, with `BuildCommand(opts)` and
declarative settings). For Phase 1 the adapter carries four things:

```rust
// illustrative
trait AgentAdapter {
    fn id(&self) -> &str;                 // "codex" | "claude"
    fn launch(&self, opts: &LaunchOpts) -> Command;   // argv + env + cwd
    fn input_rules(&self) -> InputRules;  // bracketed paste + submit delay
    fn manifest(&self) -> &StateManifest; // loaded from TOML
    fn resume(&self, sref: &SessionRef) -> Command;   // resume argv
    fn install_hook(&self) -> io::Result<()>;         // SessionStart hook for session-id
}
```

### Per-agent specifics (from the source)

| Aspect | Claude Code | Codex |
|---|---|---|
| Launch | `claude` (PTY/TUI) | `codex` (PTY/TUI) |
| Resume argv | `claude --resume <id>` | `codex resume <id>` |
| Session dir | `~/.claude` | `~/.codex` |
| Input injection | **Ink TUI**: bracketed paste OFF; send `\r` as a separate write after **~150 ms** delay (else the Enter is swallowed) | bracketed paste ON for multi-line; submit `\r` |
| Blocked signal | screen form: "enter to select" + "esc to cancel" + nav hints | OSC title contains "Action Required" |
| Working signal | Braille spinner (U+2800–U+28FF) in OSC title | Braille spinner in OSC title |

References: input quirks — `kandev/.../agent/agents/claude_acp.go:54-72`,
`passthrough_payload.go`; resume argv — `herdr/src/agent_resume.rs:113-188`;
detection rules — `herdr/src/detect/manifests/{claude,codex}.toml`.

## State Engine

Three layers, exactly as herdr (`herdr/src/detect/`, `herdr/src/pane/`):

### 1. OSC parser (primary signal)
- Incremental, stateful: OSC `0`/`2` (title), `9` (progress), terminated by BEL
  (`\x07`) or ST (`\x1b\\`). Buffer partial sequences across reads.
- Clear OSC evidence on foreground-process change (`herdr/src/pane/osc.rs`,
  `terminal.rs:466-514`).

### 2. Screen-content manifest matcher (fallback)
- Run the VT emulator over PTY output; extract the visible screen.
- Match the screen tail against TOML rules. Rule shape (herdr
  `src/detect/manifest.rs`, `manifests/*.toml`):
  ```toml
  [[rules]]
  id = "live_blocked_form"
  state = "blocked"
  priority = 980
  region = "after_last_horizontal_rule"
  visible_blocker = true
  contains = ["enter to select", "esc to cancel"]
  any = [{ contains = ["arrow keys to navigate"] }, { contains = ["↑/↓ to navigate"] }]
  ```
- Regions: `osc_title`, `osc_progress`, `whole_recent`, `bottom_lines(N)`,
  `bottom_non_empty_lines(N)`, `after_last_prompt_marker`, `prompt_box_body`,
  `after_last_horizontal_rule`.
- Gates: `contains` (case-insensitive substring), `regex`, `line_regex`, plus
  recursive `all` / `any` / `not`. Highest `priority` wins.
- Cap complexity (herdr): ≤128 rules, ≤512 gates, ≤1024 matchers, depth ≤8.
- Handle variable-width (CJK) glyphs when slicing regions (herdr fixed off-by-one
  here).

### 3. PTY activity + debounced state machine
- Bytes flowing = working. Idle transition is held until confirmed. Timing
  constants (copy from herdr `src/pane/agent_detection.rs:5-13`):
  - recheck `100 ms`, require `3` confirmations, cap `700 ms`;
  - stable-visible refresh `800 ms`; startup grace `3 s`.
- Only publish a transition that passes the stability window and visible-UI gate.

Each published state carries `source` ∈ {`osc_title`, `osc_progress`, `screen`,
`process`}.

> Kandev independently confirms this shape: a `StatusDetector.DetectState(lines,
> glyphs)` over a vt10x emulator with a `StabilityWindow`
> (`kandev/.../agentctl/server/process/status_tracker.go`). Its per-agent
> detector bodies were not in the files read — treat the `blocked` rules as the
> empirical part to tune.

## Hook Integration (Session-ID Capture Only)

Hooks do **not** report live state. They capture the native session ID for
resume (herdr `src/integration/`, `assets/claude/herdr-agent-state.sh`):

- The daemon injects env before spawning the agent: `ZAGENTMESH_SOCKET_PATH`,
  `ZAGENTMESH_SESSION_ID`.
- Claude: install a `SessionStart` hook in `~/.claude/settings.json` pointing at
  a small script; remove any stale lifecycle hooks. Codex: configure `notify`
  equivalently.
- The hook reads its stdin JSON (`session_id`, `transcript_path`), then sends one
  fire-and-forget RPC (0.5 s timeout) to the socket:
  `session.report_native_id {session_id(zagentmesh), agent, native_session_id, transcript_path?}`.
- The daemon stores it as the session's resume binding.

## Resume Model

- `SessionRef { kind: Id | Path, value }` with validation: non-empty, no control
  chars, ≤512 (id) / ≤4096 (path), path absolute (herdr
  `src/agent_resume.rs:8-70`).
- Resume command built per agent (table above).
- Durability: client detach/restart → reattach live PTY. Daemon restart → PTY is
  gone; resume via stored native ID (`claude --resume`, `codex resume`). Document
  that a daemon upgrade kills live PTYs by design.

## Worktree-per-Session

Model from Kandev (`kandev/.../worktree/worktree.go:24-115`):

- Bind one worktree per `(session_id, repository, branch)`; store
  `path, branch, base_branch, status` (`active`/`merged`/`deleted`).
- Ownership check before reuse/cleanup.
- Non-fatal warnings (fetch failure, base-branch fallback, setup-script failure):
  keep the worktree, surface the warning, let the user decide.

## SQLite Schema (initial, `user_version = 1`)

```sql
CREATE TABLE session (
  id TEXT PRIMARY KEY,
  agent TEXT NOT NULL,                 -- 'codex' | 'claude'
  cwd TEXT NOT NULL,
  repo TEXT, base_branch TEXT, branch TEXT, worktree_path TEXT,
  pty_cols INTEGER, pty_rows INTEGER,
  state TEXT NOT NULL, state_source TEXT,
  native_session_id TEXT, native_session_path TEXT,  -- resume binding
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  exited_at TEXT, exit_code INTEGER
);
CREATE TABLE worktree (
  id TEXT PRIMARY KEY, session_id TEXT, repository TEXT,
  branch TEXT, base_branch TEXT, path TEXT, status TEXT,
  created_at TEXT, merged_at TEXT, deleted_at TEXT
);
```

Event log is a separate append-only file under `events/` (JSON lines); never
stores secrets. `state.db` is rebuildable from sources; the event log is the
audit/debug trail.

## CLI Grammar (forward-compatible with Phase 2)

- Local now: `zagentmesh session new --agent claude`, `zagentmesh attach s-42`.
- Design the host-target syntax now so Phase 2 adds it without breaking:
  `zagentmesh attach <host>/<session-id>`, `--host <host>` on commands. Phase 1
  accepts only the local form but the parser is host-aware.
- `--json` on `list`, `inspect`, `status`, and all automation commands.
- `zagentmesh doctor`: check Codex/Claude binaries, git, socket dir perms,
  state-dir writability, schema version.

## Logging and Observability

- `tracing` JSON logs under `~/.local/state/zagentmesh/logs/`, redacting secrets
  and terminal content.
- Log: daemon start/stop, single-instance/socket recovery, control requests (with
  `id`) + status, session start/attach/detach/stop/exit/resume, PTY
  alloc/resize/stream errors, state transitions with `source`.

## Build Order (milestones with checkpoints)

1. **Workspace + protocol crate.** Envelopes, version negotiation, typed errors.
   *Check:* round-trip serde unit tests pass.
2. **Daemon skeleton + Unix socket server.** `daemon.health`, single-instance
   lock, stale-socket recovery, `0600` socket. *Check:* CLI `doctor` + `daemon
   start` talk over the socket.
3. **PTY actor + `session.new` (shell).** Spawn `/bin/sh` in a PTY; supervise.
   *Check:* session appears in `list`; survives client exit.
4. **Attach stream.** Separate connection; resize/detach over control. *Check:*
   attach a shell, type, detach, reattach; binary output survives.
5. **VT + OSC parser + manifest matcher + state machine.** *Check:* fixtures map
   to working/idle/blocked; debounce kills flicker.
5b. **Attach history replay.** Bounded per-session output ring buffer (default
    10 MB); replay recent screen + scrollback to a new client on attach, taken
    atomically with the live subscription. *Check:* reattach a session and
    receive its prior output with no input sent; no gap or duplicate.
6. **Agent adapters (Codex + Claude).** Launch, input injection (Claude Ink
   quirk!), manifests, resume command. *Check:* run both agents end-to-end, inject
   a prompt that actually submits, observe correct state.
7. **Session-ID hook + resume.** Install hook, capture native id, `resume` after
   daemon restart. *Check:* kill daemon, restart, resume both agents.
8. **Worktree-per-session.** Bind/ownership/warnings. *Check:* two sessions, two
   worktrees, no shared tree.
9. **SQLite persistence + migration test + event log.** *Check:* metadata
   survives restart; a `user_version` 1→2 migration applies cleanly.
10. **`--json` everywhere + polish errors/recovery hints.** *Check:* JSON parses;
    missing-binary and version-mismatch errors are clear.

## Open Questions to Validate Empirically (early)

- The exact `blocked`/awaiting-approval signal per agent and agent-CLI version
  (OSC vs screen form). Build fixtures from real sessions first; tune manifests.
- Claude Ink submit delay value (start at 150 ms per Kandev; confirm).
- Whether `vt100` is sufficient for screen extraction or `alacritty_terminal` is
  preferable (run both against the same fixtures in milestone 5).
- Codex `notify` hook payload shape for native-session-id capture (confirm
  against the installed Codex version).
