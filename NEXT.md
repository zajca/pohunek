# NEXT STEP — Milestone 7: Session-ID Hook + Resume

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

- Authoritative spec: [`docs/plan-phase-1.md`](docs/plan-phase-1.md) — see
  "Hook Integration (Session-ID Capture Only)", "Resume Model", the
  `AgentKind` adapter table, the "SQLite Schema" (where the resume binding
  eventually lives — milestone 9, not here), and Build-Order milestone 7
  ("Session-ID hook + resume": install hook, capture native id, resume after
  daemon restart; *Check:* kill daemon, restart, resume both agents).
- Phase scope: [`docs/phases/01-core-local-sessions.md`](docs/phases/01-core-local-sessions.md)
  ("After a daemon restart, sessions resume via captured native session IDs",
  the per-agent detection signals, "Risks").
- Reference source (vendored locally at `/tmp/herdr`): **herdr**
  `src/integration/mod.rs` (`install_claude` ≈ line 1453, `install_codex` ≈
  line 1514: write the hook script, merge it into `~/.claude/settings.json`
  `hooks.SessionStart` / the Codex `hooks.json`, strip stale lifecycle hooks),
  `src/integration/assets/{claude,codex}/herdr-agent-state.sh` (the hook
  scripts to port — read stdin JSON, fire one RPC to the socket),
  `src/agent_resume.rs:8-70` (the `SessionRef { Id | Path }` validation to
  port). **Kandev** is not needed this milestone.

---

## Where we are now (done, verified)

Milestones 1–6 are complete (`cargo build`,
`cargo clippy --all-targets --workspace -D warnings`, `cargo test --workspace`
= 203 passed):

- `crates/protocol` — typed control envelopes; session lifecycle + attach types;
  `AgentKind { Shell | Codex | Claude }`; `AgentActivity { Working|Blocked|Idle }`
  + `SessionInfo.activity`; `StateSource { OscTitle|OscProgress|Screen|Process }`;
  the `agent_state` event; `session.input` (`SessionInputParams`/`Result`).
  `method::SESSION_REPORT_NATIVE_ID` (`session.report_native_id`) is **declared
  but not handled** — this is the milestone you wire up here.
- `crates/daemon` (`zagentmeshd`) — Unix-socket server; full `session.*`
  lifecycle (incl. `session.input`), attach bridge, `subscribe` event stream;
  in-memory `SessionRegistry` owning one PTY per session; a per-session
  detection task running the state engine and publishing `agent_state`.
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon`, `health`/`status`, `session`
  group (`new --agent shell|codex|claude`, `list`, `inspect`, `stop`, `input`),
  `attach <target>`.
- **State engine (M5):** `daemon/src/detect/` = `osc`, `screen`, `manifest`
  (TOML matcher), `machine` (debounce). Generic shell + per-agent Codex/Claude
  manifests embedded via `include_str!` and selected by `AgentKind` through
  `DetectorConfig::for_agent`.
- **Agent adapters (M6):** `daemon/src/agent/` = the `AgentAdapter` trait
  (`id`, `launch(&LaunchOpts) -> PtyCommand`, `input_rules() -> InputRules`,
  `manifest() -> &Manifest`, `resume(&SessionRef) -> AgentCommand`) + `codex.rs`
  / `claude.rs`. `create_session` launches per `AgentKind`; missing binary on
  `PATH` → typed `agent_binary_missing`. Input injection honors `InputRules`
  (Claude Ink: bracketed-paste OFF + `\r` after a configurable ~150 ms delay;
  Codex: bracketed-paste ON). The **resume-argv builder is built and unit-
  tested** (`claude --resume <id>` / `codex resume <id>`) but **not wired**.
- Stubs (TODO doc-comment only, NOT implemented):
  `daemon/src/{store,events}/mod.rs`. The store stub names "resume bindings"
  as eventual SQLite content (milestone 9).

### Seams milestone 7 builds on (already in place)

- `crates/protocol` — `method::SESSION_REPORT_NATIVE_ID` is declared but has no
  param type and no handler. M7 adds `SessionReportNativeIdParams` and a minimal
  result, and dispatches it (mirror the `session.input` param/result/handler
  shapes added in M6).
- `daemon/src/agent/mod.rs` — `SessionRef` exists as an **id-only** string
  (`new`, validated: non-empty, ≤512 bytes, no control chars) feeding the
  resume builder. The plan's Resume Model wants `SessionRef { kind: Id | Path }`
  with a **Path** variant (≤4096 bytes, absolute). M7 extends `SessionRef`
  additively; `resume()` keeps using `.value()`.
- `daemon/src/agent/mod.rs` — `LaunchOpts.env_extra` is plumbed end-to-end but
  `create_session` passes an **empty** `env_extra` (`session/mod.rs`). M7 fills
  it with the hook-handshake env so the spawned agent's hook can call home.
- `daemon/src/{store,events}/mod.rs` — empty stubs. M7 fills a **minimal**
  resume-binding store here (see scope note below). Full SQLite + event log
  stay milestone 9.
- The CLI `session` group and `doctor` are the surfaces for a hook-install
  command and (optionally) a manual `session resume`.

---

## Goal of milestone 7

Capture each agent's **native session id** through a `SessionStart` hook and use
it to **resume after a daemon restart**. The daemon installs a small hook into
Claude's (`~/.claude/settings.json`) and Codex's config; when an agent starts it
fires one RPC back to the socket carrying its native session id; the daemon
records that as the session's resume binding. After the daemon is killed and
restarted, the live PTYs are gone (by design), so the daemon rebuilds each
session from its stored binding and resumes the agent via the M6 resume-argv
builder.

Hooks are **session-id capture only** — they do **not** report live state
(state still comes from the M5 detector). **Still local, single-host.**

> **Scope note on persistence (read this).** Build-Order step 7's checkpoint
> ("kill daemon, restart, resume both agents") needs the resume binding to
> survive a process restart, but full **SQLite persistence + event log +
> migration test is milestone 9**. So M7 introduces a **minimal, single-purpose
> resume-binding store** (a JSON-lines / small file under the daemon state dir,
> holding only what is needed to relaunch-and-resume: `session_id`, `agent`,
> `cwd`, `cols`, `rows`, `native_session_id`, `native_session_path?`). It is an
> explicit **precursor** to the M9 `session` table — fill `store/mod.rs` with
> this minimal store and leave a `TODO(milestone 9)` that SQLite absorbs it. Do
> **not** build the full schema, the worktree table, or the `events/` log here.

### Definition of done (testable)

1. `method::SESSION_REPORT_NATIVE_ID` gains a `SessionReportNativeIdParams
   { session_id, agent, native_session_id, transcript_path? }` and a minimal
   result (e.g. `{ recorded: bool }` — the hook fires-and-forgets and ignores
   the body). Round-trip tests in `protocol/tests/roundtrip.rs`.
2. `SessionRef` becomes `{ kind: Id | Path, value }` (port the validation from
   herdr `src/agent_resume.rs:8-70`: id ≤512, path ≤4096 + absolute, both
   non-empty + no control chars). The M6 resume builders keep producing
   `claude --resume <value>` / `codex resume <value>`. Unit tests cover both
   kinds + each rejection.
3. The daemon injects the hook-handshake env before spawning every Codex/Claude
   agent: `ZAGENTMESH_SOCKET_PATH` (the control socket) and
   `ZAGENTMESH_SESSION_ID` (the zagentmesh session id), via `LaunchOpts.env_extra`.
   (Shell sessions get no hook env.)
4. A hook installer (`daemon/src/integration/`) writes the per-agent hook script
   and registers it: Claude → `~/.claude/hooks/` + merge into `settings.json`
   `hooks.SessionStart` (matcher `*`), stripping any stale lifecycle hooks it
   owns; Codex → its config dir + `hooks.json` (the `notify`/`SessionStart`
   equivalent). Reinstall is **idempotent** (no duplicate entries) and never
   clobbers unrelated user settings. Ported from herdr but emitting **our**
   env names + **our** `session.report_native_id` method.
5. The hook script reads its stdin JSON (`session_id`, `transcript_path`), and
   if `ZAGENTMESH_ENV`/socket/session-id are present, sends one fire-and-forget
   RPC (0.5 s timeout) `session.report_native_id` to the socket. Missing env or
   missing python/runtime → silent no-op exit 0 (never break the agent).
6. The daemon **handles** `session.report_native_id`: validate, build a
   `SessionRef`, and write the resume binding into the minimal store (and update
   the in-memory `SessionInfo` so `inspect`/`list` can show it). Reports for an
   unknown/terminal session are ignored, not errors.
7. **Restart-resume:** on startup the daemon loads the resume-binding store; a
   `resume` path relaunches a session with the agent's resume argv (M6 builder)
   in the stored `cwd`/size, attaches a fresh PTY + detector, and re-registers
   it. Document (code comment + `docs`) that a daemon restart kills live PTYs by
   design and only resumable sessions (those with a captured native id) come
   back.
8. CLI: a hook-install command (e.g. `zagentmesh integration install
   [--agent claude|codex]`) and surfacing the native id / "resumable" in
   `inspect` (and optionally a manual `session resume <target>`). Display-only
   formatter branches unit-tested.
9. End-to-end (extend `crates/daemon/tests/health_socket.rs`): a stub `claude`
   and `codex` that, on launch, call `session.report_native_id` (simulating the
   hook), then idle. Assert the binding is recorded; then **drop the registry
   (simulated daemon kill), rebuild it against the same state dir, resume**, and
   assert the relaunched stub received the resume argv (`--resume <id>` /
   `resume <id>`).
10. `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
    `cargo test --workspace` stay clean.

### Explicitly OUT of scope (later milestones — do NOT build here)

- **Live-state hooks.** Hooks capture the native id only; state stays with the
  M5 detector. Do not wire `PostToolUse`/`Stop`/etc. into activity.
- **Full SQLite persistence + `events/` append-only log + `user_version` 1→2
  migration test** → **milestone 9**. M7 ships only the minimal resume-binding
  store described in the scope note.
- **Worktree-per-session** → milestone 8.
- **`--json` everywhere + error/recovery polish** → milestone 10.

---

## Implementation tasks

### 1. `crates/protocol` — the native-id report

- Add `SessionReportNativeIdParams { session_id: SessionId, agent: AgentKind,
  native_session_id: String, #[serde(default)] transcript_path: Option<String> }`
  and a minimal result. Mirror the `session.input` param/result shapes.
- Round-trip tests for the params (with and without `transcript_path`) and the
  result.

### 2. `crates/daemon/src/agent/` — `SessionRef` kind + path

- Extend `SessionRef` to `{ kind: Id | Path, value }`; add a `path` constructor
  (≤4096 bytes, absolute, non-empty, no control chars) alongside the existing id
  constructor (≤512). Keep `value()`; `resume()` is unchanged.
- Unit tests: id/path accept + each rejection (empty, control char, over-length,
  relative path).

### 3. `crates/daemon/src/integration/` — hook install (new module)

- Port herdr's hook scripts (`assets/{claude,codex}/herdr-agent-state.sh`) into
  embedded assets, rewritten to use **our** env names (`ZAGENTMESH_ENV`,
  `ZAGENTMESH_SOCKET_PATH`, `ZAGENTMESH_SESSION_ID`) and **our** method
  (`session.report_native_id`). Keep the fire-and-forget + 0.5 s timeout + silent
  no-op-on-missing-env behavior.
- `install_claude` / `install_codex` (model: herdr `install_claude` ≈ 1453,
  `install_codex` ≈ 1514): write the script, `chmod +x`, merge into the agent's
  settings (`~/.claude/settings.json` `hooks.SessionStart` matcher `*`; Codex
  `hooks.json`), idempotently, stripping only hooks this installer owns. Fail
  fast with a typed error if the agent's config dir is absent.
- Unit-test the merge against fixture settings (fresh file, pre-existing
  unrelated hooks, and a reinstall) — assert no duplicates and unrelated keys
  preserved.

### 4. `crates/daemon/src/session/` + `store/` — env, capture, store, resume

- `create_session`: for Codex/Claude, populate `LaunchOpts.env_extra` with
  `ZAGENTMESH_ENV=1`, `ZAGENTMESH_SOCKET_PATH`, `ZAGENTMESH_SESSION_ID` (the
  daemon must know its own socket path; thread it through
  `SessionRegistryConfig`). Shell sessions: no hook env.
- Handle `session.report_native_id`: validate, build the `SessionRef`, write the
  binding to the minimal store, update `SessionInfo` (native id visible to
  `inspect`/`list`). Unknown/terminal session → ignore.
- Fill `store/mod.rs` with the minimal resume-binding store (load/save; small
  file under the daemon state dir; `TODO(milestone 9)` that SQLite absorbs it).
  No secrets are ever written.
- A `resume` entry point: read a binding, relaunch via the M6 resume argv in the
  stored `cwd`/size, wire a fresh detector, re-register. On daemon startup, load
  the store so resumable sessions are known.

### 5. `crates/cli` — install + surface resume

- A hook-install command (`integration install [--agent ...]`) calling the
  daemon (or a local install path — match how `doctor` reaches config).
- Surface the native id / "resumable" in `inspect` (and optionally
  `session resume <target>`). Unit-test any new formatter branch.

---

## Tests (must pass before done)

Most layers are unit-testable without a live agent; use stub scripts for the
hook + resume round trip.

- protocol: round-trip for `SessionReportNativeIdParams` (with/without
  `transcript_path`) and the result.
- agent: `SessionRef` id + path accept/reject; resume argv for an id and a path.
- integration: hook-script asset shape (fires our method, uses our env, exits 0
  on missing env); settings merge is idempotent and preserves unrelated keys.
- session/store: `report_native_id` records the binding and updates
  `SessionInfo`; the minimal store round-trips load/save; env-injection present
  at launch for Codex/Claude and absent for Shell.
- daemon integration (extend `crates/daemon/tests/health_socket.rs`): a stub
  `claude`/`codex` that calls `session.report_native_id` on launch; assert the
  binding; then rebuild the registry against the same state dir and resume;
  assert the relaunched stub argv is `--resume <id>` / `resume <id>`.
- Keep `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
  `cargo test --workspace` clean.

---

## After this milestone

Milestone 8 = **worktree-per-session** (bind one worktree per
`(session_id, repo, branch)`; ownership check before reuse/cleanup; non-fatal
warnings on fetch/base-branch/setup-script failure — model: Kandev
`worktree/worktree.go:24-115`). Then milestone 9 = **SQLite persistence +
append-only event log + `user_version` 1→2 migration test** (this is where the
minimal M7 resume-binding store is absorbed into the `session` table and the
`worktree` table lands; `state.db` rebuildable from sources, event log the audit
trail). Then milestone 10 = **`--json` everywhere + error/recovery polish**.

Empirical open questions to settle now (per the plan): the exact Claude and
Codex `SessionStart` hook stdin JSON shapes (record from real sessions — Claude
gives `session_id` + `transcript_path`; Codex gives `session_id`); whether Codex
exposes its native session id via `notify`/`hooks.json` the same way Claude does
(confirm against a live Codex session); and that a daemon restart cleanly
relaunches `claude --resume` / `codex resume` against a real captured id.

Do not pull milestone 8+ worktree/persistence work into this step — keep M7
proven: both agents install a hook, both report a native id on start, the
binding survives a registry rebuild, and both resume via the M6 argv builder.
