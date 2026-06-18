# NEXT STEP — Milestone 6: Agent Adapters (Codex + Claude Code)

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

- Authoritative spec: [`docs/plan-phase-1.md`](docs/plan-phase-1.md) — see
  "Agent Adapter Boundary" (the trait + per-agent table), "State Engine"
  (manifests plug into the matcher), "Hooks" / "Resume Model" (the parts that
  start here vs. land in milestone 7), and Build-Order milestone 6.
- Phase scope: [`docs/phases/01-core-local-sessions.md`](docs/phases/01-core-local-sessions.md)
  ("Prompts inject correctly, including Claude Code's Ink submit quirk", the
  per-agent detection signals, "Risks").
- Reference source (vendored locally at `/tmp/herdr`): **herdr**
  `src/detect/manifests/{claude,codex}.toml` (the real per-agent state rules to
  port), `src/agent_resume.rs:113-188` (resume argv). **Kandev** is the model
  for the adapter trait and input quirks: `agent/agents/agent.go:26-64`
  (`BuildCommand`), `agent/agents/claude_acp.go:54-72` + `passthrough_payload.go`
  (Ink submit-delay + bracketed-paste handling).

---

## Where we are now (done, verified)

Milestones 1–5 are complete and merged to `main` (`cargo build`,
`cargo clippy --all-targets --workspace -D warnings`, `cargo test --workspace`
= 164 passed):

- `crates/protocol` — typed control envelopes; session lifecycle + attach types;
  `AgentActivity { Working|Blocked|Idle }` + `SessionInfo.activity`;
  `StateSource { OscTitle|OscProgress|Screen|Process }`; the `agent_state`
  event. `AgentKind` currently has **only `Shell`**. `SessionNewParams.agent:
  AgentKind` already exists. `method::SESSION_REPORT_NATIVE_ID`
  (`session.report_native_id`) is **declared but not handled** (milestone 7).
- `crates/daemon` (`zagentmeshd`) — Unix-socket server; full `session.*`
  lifecycle, attach bridge, `subscribe` event stream; in-memory
  `SessionRegistry` owning one PTY per session; a per-session **detection task**
  (second output-broadcast consumer) running the M5 state engine and publishing
  `agent_state`.
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon`, `health`/`status`, `session`
  group (with `--agent` arg already wired to `AgentKind`), `attach <target>`.
- **State engine (M5):** `daemon/src/detect/` = `osc` (OSC 0/2/9, BEL/ST,
  cross-chunk reassembly), `screen` (`vt100` grid + region slicing incl. the
  derived regions `after_last_prompt_marker` / `prompt_box_body` /
  `after_last_horizontal_rule`, CJK-safe), `manifest` (TOML matcher:
  `contains`/`regex`/`line_regex` + `all`/`any`/`not`, priority, complexity
  caps), `machine` (debounce). A shipped generic shell manifest
  (`detect/manifests/shell.toml`, embedded via `include_str!`) is loaded by the
  production detector through `DetectorConfig::generic_shell()`.
- Stubs (TODO doc-comment only, NOT implemented):
  `daemon/src/{agent,store,events}/mod.rs`. **`agent/mod.rs` is the module you
  fill in here.**

### Seams milestone 6 builds on (already in place)

- `daemon/src/agent/mod.rs` — empty stub. This is where the `AgentAdapter`
  trait + `codex.rs` + `claude.rs` live.
- `daemon/src/detect/` — the manifest matcher and the **`include_str!` +
  `DetectorConfig::generic_shell()` pattern** are the template. Milestone 6 adds
  `detect/manifests/{codex,claude}.toml` the same way and selects the manifest
  **by `AgentKind`** (generic shell stays the fallback for `Shell`).
  `manifest.rs` already parses `visible_blocker` but leaves it **unused** — M6
  is where it becomes the blocked "visible-UI gate".
- `daemon/src/session/mod.rs` — `create_session` currently launches a fixed
  shell command (`SessionRegistryConfig.shell_command`). M6 routes launch
  through the adapter (argv/env/cwd per `AgentKind`). The PTY input write path
  used by attach is the seam for **programmatic input injection**.
- `crates/protocol` — `AgentKind`, `SessionNewParams.agent`, the event stream.
  The publish/transport plumbing does not change; M6 adds enum variants and
  (likely) one input-injection method.

---

## Goal of milestone 6

Make the daemon launch and detect **real agents** — Codex and Claude Code —
not just a shell. A thin per-agent adapter carries four things: launch argv/
env/cwd, input-injection rules, the state manifest, and the resume-command
argv. The real per-agent manifests plug into the M5 matcher so a live Codex /
Claude session produces correct `working`/`blocked`/`idle` transitions with the
right `source`. Prompts injected into a session submit correctly, including
Claude Code's Ink Enter-swallow quirk.

**Still local, single-host. No `SessionStart` hook, no native-session-id
capture, no restart-resume, no worktrees, no persistence** — those are M7+.
M6 builds the *resume-command argv builder* but does not wire restart-resume.

### Definition of done (testable)

1. `AgentKind` gains `Codex` and `Claude`; `SessionNewParams`, the CLI
   `--agent`, and `session inspect`/`list` round-trip them. Round-trip tests in
   `protocol/tests/roundtrip.rs`.
2. An `AgentAdapter` trait (`id`, `launch(&LaunchOpts) -> Command`,
   `input_rules() -> InputRules`, `manifest() -> &Manifest`,
   `resume(&SessionRef) -> Command`) with `codex.rs` and `claude.rs`
   implementations. The shell path keeps working via a shell adapter (or an
   explicit non-adapter branch — your call, but keep it uniform).
3. `session new --agent codex|claude` launches the correct binary with the
   adapter's argv/env/cwd. Missing binary on `PATH` → a typed, fail-fast error
   (no silent fallback).
4. Real per-agent manifests `detect/manifests/{codex,claude}.toml` (ported from
   herdr's rules into **our** schema) are loaded per `AgentKind` into the
   detector. `visible_blocker` is consumed: a `blocked` transition requires its
   visible-UI evidence. Detection matches the per-agent table in the plan
   (Codex: OSC title "Action Required" → blocked, Braille spinner → working;
   Claude: Ink screen form "enter to select"/"esc to cancel" → blocked, spinner
   → working).
5. **Input injection:** a control method (e.g. `session.input` carrying the
   text) writes into the session PTY honoring the adapter's `InputRules`:
   Claude (Ink) → bracketed-paste OFF and the submit `\r` sent as a **separate
   write after ~150 ms** (else Enter is swallowed); Codex → bracketed-paste ON,
   submit `\r`. The delay value lives in config (no magic number).
6. A `resume(&SessionRef) -> Command` builder yields `claude --resume <id>` /
   `codex resume <id>`. Builder + unit test only — restart-resume is M7.
7. End-to-end: with a recorded fixture (or a stub agent script emitting the real
   OSC/screen shapes), a Codex and a Claude session each publish the expected
   `agent_state` transitions with the correct `source`, and an injected prompt
   reaches the PTY in the adapter-correct framing.
8. `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
   `cargo test --workspace` stay clean.

### Explicitly OUT of scope (later milestones — do NOT build here)

- **`SessionStart` hook install + native-session-id capture +
  `session.report_native_id` handling + resume *after daemon restart*** →
  **milestone 7**. (M6 builds only the resume-argv builder.)
- **Worktree-per-session** → milestone 8.
- **SQLite persistence + event-log file + migration test** → milestone 9.
- **`--json` everywhere + error/recovery polish** → milestone 10.

---

## Implementation tasks

### 1. `crates/protocol` — agent kinds + input-injection method

- Add `Codex` and `Claude` to `AgentKind` (snake_case `#[serde]`).
- Add the input-injection method: `method::SESSION_INPUT` (`session.input`) and
  a `SessionInputParams { session_id, text }` (+ result). Mirror the existing
  `session.*` param/result shapes.
- Round-trip tests for the new `AgentKind` variants, params, and result.

### 2. `crates/daemon/src/agent/` — the adapter trait + Codex/Claude

Fill the stub. Keep adapters small and independently testable.

- `AgentAdapter` trait per the plan (`docs/plan-phase-1.md` "Agent Adapter
  Boundary"). `LaunchOpts { cwd, cols, rows, env_extra }`, `InputRules {
  bracketed_paste: bool, submit_delay: Duration }`.
- `codex.rs`, `claude.rs`: launch argv/env/cwd (Claude: `claude`; Codex:
  `codex`), `input_rules` per the table (Claude Ink: paste OFF + ~150 ms submit
  delay; Codex: paste ON), `manifest()` returning the embedded per-agent
  manifest, `resume()` argv (`claude --resume <id>` / `codex resume <id>`).
- Resolve the binary from `PATH`; if absent, return a typed error — fail fast,
  no invented default path.

### 3. `crates/daemon/src/detect/manifests/` — real per-agent manifests

- Add `codex.toml` and `claude.toml`, porting herdr's rules
  (`/tmp/herdr/src/detect/manifests/{codex,claude}.toml`) into **our** manifest
  schema (note herdr's extra flags `visible_working`/`visible_idle`/
  `skip_state_update` are NOT in our schema — fold them into priority/region
  choices or extend the schema deliberately if truly needed). Embed via
  `include_str!` exactly like `shell.toml`.
- Select the manifest by `AgentKind` in `DetectorConfig` (add
  `DetectorConfig::for_agent(AgentKind)`); `Shell` keeps `generic_shell()`.
- Consume `visible_blocker`: carry it on `ManifestMatch` and require its
  evidence before the machine publishes a `blocked` transition.
- **Build fixtures from real sessions first** (record actual Codex/Claude PTY
  output) and unit-test the manifests against them — do not hand-wave the rules.

### 4. `crates/daemon/src/session/mod.rs` — launch via adapter + inject input

- `create_session` selects the adapter by `params.agent` and launches the PTY
  with the adapter's `Command`; wire `DetectorConfig::for_agent(params.agent)`
  into `spawn_detector`.
- Handle `session.input`: write `text` to the session PTY honoring the
  adapter's `InputRules` (bracketed-paste wrapping; for Ink, spawn a short timer
  task to send `\r` as a separate write after `submit_delay`). Do not block the
  control connection on the delay.

### 5. `crates/cli` — surface agents + a prompt command

- `session new --agent codex|claude` (the `--agent` arg already maps to
  `AgentKind`; add the variants). Show the agent in `list`/`inspect`.
- A small `session input <target> <text>` (or `prompt`) command calling
  `session.input`. Display-only otherwise; unit-test any new formatter branch.

---

## Tests (must pass before done)

Most layers are unit-testable without a live agent; record fixtures for the
detection rules.

- protocol: round-trip for the new `AgentKind` variants, `SessionInputParams`,
  result.
- agent: adapter `launch` argv/env/cwd per agent; `resume` argv; `input_rules`
  values; missing-binary → typed error.
- detect: per-agent manifest parses; precedence and `visible_blocker` gating
  against **recorded screen/OSC fixtures** (Codex "Action Required",
  Braille-spinner working; Claude Ink blocked form, spinner working, idle).
- session input injection: bracketed-paste framing per agent; the Ink
  submit-delay timer sends `\r` as a separate write after the configured delay
  (time-driven unit test, like `machine.rs`).
- daemon integration (extend `crates/daemon/tests/health_socket.rs`): launch a
  stub agent script that emits the real OSC/screen shapes; assert the expected
  `agent_state` transitions + `source` and that an injected prompt reaches the
  PTY.
- Keep `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
  `cargo test --workspace` clean.

---

## After this milestone

Milestone 7 = **`SessionStart` hook + native-session-id capture + restart-
resume**: install a Claude `SessionStart` hook and the Codex equivalent
(herdr `src/integration/`, `assets/claude/herdr-agent-state.sh`); handle
`session.report_native_id` and store the resume binding; after a daemon restart,
rebuild sessions and resume via the M6 resume-argv builder. Then milestone 8
(worktree-per-session), 9 (SQLite persistence + event log + migration test),
10 (`--json` everywhere + error/recovery polish).

Empirical open questions to settle now (per the plan): the exact Claude Ink
submit-delay value (start at **150 ms**, confirm empirically — Enter is
swallowed if too short); the precise blocked/awaiting-approval signal per agent
(OSC title vs. screen form) — **build fixtures from real Codex/Claude sessions
first**; Codex's native-session-id capture payload shape (relevant for M7, but
record it now while you have live sessions).

Do not pull milestone 7+ hook/resume/worktree/persistence work into this step —
keep M6 proven on real agents: both launch, both detect correctly with sources,
prompts inject and submit (Ink quirk included), and the resume argv is built.
