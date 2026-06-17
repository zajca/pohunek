# NEXT STEP — Milestone 5: State Engine (VT + OSC + manifest + debounce)

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

- Authoritative spec: [`docs/plan-phase-1.md`](docs/plan-phase-1.md) — see the
  "State Engine" section (three layers), "Tech Stack and Crates" (VT/OSC/manifest
  deps), "Logging and Observability" (state transitions with `source`), and
  Build-Order milestone 5.
- Phase scope: [`docs/phases/01-core-local-sessions.md`](docs/phases/01-core-local-sessions.md)
  ("Agent state from the terminal stream", "Testing and Verification", "Risks").
- Reference source: **herdr** `src/detect/` (OSC parser, manifest matcher, state
  machine) and `src/pane/` (`osc.rs`, `terminal.rs:466-514`,
  `agent_detection.rs:5-13`). **Kandev** independently confirms the shape:
  `StatusDetector.DetectState(lines, glyphs)` over a vt10x emulator with a
  `StabilityWindow` (`agentctl/server/process/status_tracker.go`). Same Rust +
  `portable-pty` + Tokio stack as ours.

---

## Where we are now (done, verified)

Milestones 1–4 are complete, green, and verified (`cargo test --workspace` = 64
passed, `cargo clippy --all-targets --workspace` clean, `cargo build` clean):

- `crates/protocol` — typed control envelopes (request / response ok|err /
  event), `ProtocolError`, `PROTOCOL_VERSION = 1`, `negotiate()`. Session
  lifecycle + attach types in `src/session.rs` (`SessionNewParams`, `SessionId`,
  `SessionInfo`, `SessionState`, `SessionStopResult`, `SessionAttach/Detach/
  ResizeParams` + results, `AttachHeader`, `AgentKind = { Shell }`). Event names
  include `attach_opened`/`attach_closed`. **`StateSource` already exists**
  (`OscTitle | OscProgress | Screen | Process`) and `SessionInfo.state_source` is
  already wired — milestone 5 is the first code that actually *produces* a
  non-`Process` source.
- `crates/daemon` (`zagentmeshd`) — Tokio Unix-socket server; `daemon.health`,
  the `session.*` lifecycle methods, `session.attach/detach/resize`, raw attach
  bridge (separate connection, framed→raw handover, multi-client), and a
  `subscribe` connection that streams control events. In-memory
  `SessionRegistry` owns one PTY per session.
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon start [--detach]`,
  `health`/`status`, the `session` group, and `attach <target>` (raw terminal
  client: termios raw mode, SIGWINCH→`session.resize`, Ctrl-] detach).
- Stubs (TODO doc-comment only, NOT implemented):
  `daemon/src/{agent,detect,store,events}/mod.rs`. **`detect/mod.rs` is the
  module you fill in here.**

### Seams milestone 5 builds on (already in place)

- `daemon/src/pty/mod.rs` `PtyHandle::subscribe_output() ->
  broadcast::Receiver<Vec<u8>>` — **the detection input seam.** This is the
  *same* broadcast the attach stream consumes. Milestone 5 adds a *second*
  independent consumer per session: a detection task. Each ~8 KB PTY read chunk
  arrives here. (Capacity is 64; a lagging detector must log `Lagged` and resync,
  never silently drop — same rule as the attach bridge.)
- `daemon/src/session/mod.rs` `SessionRegistry` — owns `SessionEntry { info,
  pty, stopping }`; emits `session_*` events via the `events` broadcast and
  `emit()`/`emit_attach()`. This is where the detector publishes state changes.
- `crates/protocol` `StateSource` + `SessionInfo.state_source` + the
  `subscribe` event stream — the publish path. The detector updates
  `SessionInfo` and emits an event the same way `resize` already does.

---

## Goal of milestone 5

Turn raw PTY bytes into a **debounced agent-activity signal** (`working`,
`blocked`, `idle`) with a recorded `source`, published on the control event
stream and reflected in `session.inspect`. Three layers, exactly as herdr:

1. **OSC parser (primary):** incremental, stateful parse of OSC `0`/`2` (title),
   `9` (progress), terminated by BEL (`\x07`) or ST (`\x1b\\`); buffer partial
   sequences across reads.
2. **VT screen + manifest matcher (fallback):** run a VT emulator over the PTY
   output to extract the visible screen, match its tail against per-agent TOML
   rules.
3. **PTY activity + debounced state machine:** bytes flowing = working; idle/
   blocked transitions held behind a stability window so the UI never flickers.

Still a plain shell — **no agent adapters, no manifests shipped for real agents,
no resume, no persistence.** This proves the detection pipeline end-to-end on
synthetic input (shell-emitted OSC + recorded fixtures) before any agent code.

### Definition of done (testable)

1. A per-session **detection task** subscribes to
   `PtyHandle::subscribe_output()` and runs for the life of the session; it stops
   cleanly when the session reaches a terminal state (no leaked tasks).
2. **OSC parser** extracts title (OSC 0/2) and progress (OSC 9) incrementally and
   **correctly reassembles sequences fragmented across read chunks**; a BEL- and
   an ST-terminated sequence both parse; a title set via the shell
   (`printf '\033]0;working…\007'`) is observed by the daemon.
3. **VT screen extraction** turns PTY output into a visible screen grid; region
   slicing handles variable-width (CJK) glyphs without an off-by-one.
4. **Manifest matcher** evaluates TOML rules (regions + `contains`/`regex`/
   `line_regex` gates + recursive `all`/`any`/`not`, highest `priority` wins)
   against the screen tail and yields a state; complexity caps enforced
   (≤128 rules, ≤512 gates, ≤1024 matchers, depth ≤8).
5. **Debounced state machine** publishes a transition only after the stability
   window (recheck 100 ms, 3 confirmations, cap 700 ms; stable-visible refresh
   800 ms; startup grace 3 s) — recorded flicker fixtures collapse to a single
   stable transition.
6. Each published state carries `source ∈ {osc_title, osc_progress, screen,
   process}` and is visible via `session.inspect` **and** as an event on the
   `subscribe` stream.
7. The detector is a **second** consumer of the output broadcast: attaching a
   raw client (milestone 4) and detection run **simultaneously** on the same
   session without either starving the other (lag on either side is logged, not
   silently dropped).
8. Integration + unit tests cover OSC fragmentation, region slicing (incl. CJK),
   manifest precedence, debounce/anti-flicker, and the end-to-end shell-OSC →
   published-state path. `cargo build`, `cargo clippy --all-targets --workspace`,
   and `cargo test --workspace` stay clean.

### Explicitly OUT of scope (later milestones — do NOT build here)

- **Agent adapters** (Codex/Claude launch argv, Claude Ink submit-delay,
  input-injection, the real per-agent manifests) → **milestone 6**. Here: ship a
  small *generic/shell* manifest (or synthetic fixtures) only — enough to prove
  the matcher, not to detect a real agent.
- **`SessionStart` hook + native-session-id capture + resume** → **milestone 7**.
- **Worktree-per-session** → milestone 8.
- **SQLite persistence + event-log file** → milestone 9. State stays in-memory;
  published only on the live event stream and `inspect`.

---

## Implementation tasks

### 1. `crates/protocol` — agent-activity signal + `agent_state` event

`StateSource` already exists; the missing piece is the *activity* value and its
event. **Design decision to make (flag the choice):** the lifecycle
`SessionState` (`Starting|Running|Stopped|Done|Failed`) is orthogonal to agent
activity (`Working|Blocked|Idle`) — a `Running` session is independently working
or idle or blocked. **Recommended:** add a separate
`enum AgentActivity { Working, Blocked, Idle }` and an
`activity: Option<AgentActivity>` field on `SessionInfo` (None until first
detection), rather than overloading `SessionState`. Keep the existing snake_case
`#[serde]` style and `skip_serializing_if` for the option.

- Add `AgentActivity` (snake_case) and `SessionInfo.activity`
  (`skip_serializing_if = "Option::is_none"`, `#[serde(default)]`).
- Add event name `agent_state` in `pub mod event`, carrying
  `{ session_id, activity, source }` (mirror how `session_*`/`attach_*` events
  are shaped).
- Round-trip unit tests in `tests/roundtrip.rs` for `AgentActivity`, the extended
  `SessionInfo` (activity present and absent), and the `agent_state` event JSON
  shape.

### 2. `crates/daemon/src/detect/mod.rs` — the three layers

Fill in the stub. Keep the layers as separate, independently testable units
(submodules: `osc`, `screen`, `manifest`, `machine` or similar) so each has its
own unit tests.

- **OSC parser** (`osc.rs`): incremental state machine; OSC `0`/`2`→title,
  `9`→progress; terminator BEL `\x07` or ST `\x1b\\`; **buffer partial sequences
  across `recv()` chunks**. Clear OSC evidence on foreground-process change
  (herdr `pane/osc.rs`, `terminal.rs:466-514`) — for a plain shell, foreground
  tracking is minimal; document what we track now and what M6 tightens.
- **VT screen** (`screen.rs`): feed bytes to the VT emulator, expose the visible
  grid + tail-region slices. **Dependency decision (flag to the user):** use
  `vt100` (pure-Rust, low risk, recommended for Phase 1) vs `alacritty_terminal`.
  The plan lists this as an open question — pick `vt100`, but run the same
  fixtures against both if behavior is doubtful. Handle CJK width when slicing.
- **Manifest matcher** (`manifest.rs`): parse TOML rule files (`toml` + `regex`);
  regions = `osc_title|osc_progress|whole_recent|bottom_lines(N)|
  bottom_non_empty_lines(N)|after_last_prompt_marker|prompt_box_body|
  after_last_horizontal_rule`; gates = `contains` (case-insensitive) `regex`
  `line_regex` + recursive `all`/`any`/`not`; highest `priority` wins; enforce
  the complexity caps. Rule shape (herdr `src/detect/manifest.rs`,
  `manifests/*.toml`):
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
- **Debounced state machine** (`machine.rs`): bytes flowing ⇒ working; hold
  idle/blocked until the stability window confirms. Constants live in config (no
  magic numbers): recheck `100 ms`, `3` confirmations, cap `700 ms`,
  stable-visible refresh `800 ms`, startup grace `3 s` (herdr
  `pane/agent_detection.rs:5-13`). Only emit a transition that passes the window
  and the visible-UI gate. Each emitted state records its `source`.

### 3. `crates/daemon/src/session/mod.rs` — spawn + own the detector, publish state

- On session creation, **spawn a detection task** (alongside the existing exit
  monitor) that owns an `osc`+`screen`+`machine` pipeline and a
  `subscribe_output()` receiver. Track its `JoinHandle`/`CancellationToken` on
  the `SessionEntry` so `stop`/`record_exit` cancels it — reuse the
  active-attach cancellation pattern; **no leaked tasks** when the session ends.
- On each confirmed transition, update `SessionInfo.activity` +
  `state_source` + `updated_at` and emit the `agent_state` event via the same
  `events` broadcast `emit()` uses. Last-writer-wins; the debounce lives in the
  detector, not the registry.
- On `Lagged` from the broadcast, log (with skipped count) and resync the VT/OSC
  state — do not pretend bytes were seen.

### 4. `crates/cli` — surface activity (small)

- Show `activity` + `source` in `session inspect` (and `list`/`status` if
  trivial) for humans and in `--json`. No new transport; this is display only.
  Unit-test the JSON shape if you add a formatter branch.

### 5. Cargo

- Daemon: add the VT crate (`vt100`), `toml`, `regex`. Nothing for agents/hooks/
  SQLite (later milestones). CLI: none expected.

---

## Tests (must pass before done)

Most of this is **unit-testable without real agents** — the agents are M6. Drive
the pipeline with synthetic input and recorded fixtures.

- protocol: round-trip for `AgentActivity`, extended `SessionInfo`, `agent_state`
  event.
- detect unit tests (per layer):
  - **OSC fragmentation:** feed a title/progress sequence split across several
    chunks (mid-escape, mid-payload) and assert correct reassembly; both BEL and
    ST terminators; a stray/aborted sequence does not wedge the parser.
  - **region slicing:** including a CJK/wide-glyph line proves no off-by-one.
  - **manifest precedence:** two matching rules → higher `priority` wins;
    `all`/`any`/`not` nesting; `contains` case-insensitivity; complexity caps
    reject an over-budget manifest.
  - **debounce/anti-flicker:** a fixture that oscillates working↔idle within the
    window collapses to one stable published transition; startup grace suppresses
    early noise.
- daemon integration (extend `crates/daemon/tests/health_socket.rs` patterns):
  - **end-to-end OSC:** create a shell session, `subscribe`, drive a title via
    the attach stream (`printf '\033]0;…\007'`), and assert an `agent_state`
    event with `source = osc_title` arrives and `session.inspect` reflects it.
  - **detector + attach coexist:** attach a raw client and run detection on the
    same session at once; both work; lag (if any) is logged.
  - **no leaked task:** stopping the session ends the detector (assert via clean
    shutdown / no pending work).
- Keep `cargo build`, `cargo clippy --all-targets --workspace`, and
  `cargo test --workspace` clean.

---

## After this milestone

Milestone 6 = **agent adapters** (Codex + Claude Code): a thin per-agent trait
carrying launch argv/env/cwd, input-injection rules (Claude Ink: bracketed-paste
off + delayed `\r` submit, ~150 ms — confirm empirically), the real per-agent
state manifests (`manifests/*.toml`), and the resume command. It plugs the real
manifests into the matcher built here. Then milestone 7 (`SessionStart` hook +
native-session-id capture + resume after daemon restart), 8 (worktree-per-
session), 9 (SQLite persistence + event log + migration test), 10 (`--json`
everywhere + error/recovery polish).

Empirical open questions to start probing now (per the plan): the exact
`blocked`/awaiting-approval signal per agent (OSC vs screen form) — **build
fixtures from real sessions first**; `vt100` vs `alacritty_terminal` sufficiency;
the Claude Ink submit-delay value.

Do not pull milestone 6+ agent/manifest/resume work into this step — keep the
state engine proven on synthetic input first: OSC fragments reassemble, regions
slice correctly, manifests resolve by priority, debounce kills flicker, and a
shell-emitted title surfaces as a published state with its `source`.
