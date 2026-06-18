# NEXT STEP — Milestone 8: Worktree-per-Session

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

- Authoritative spec: [`docs/plan-phase-1.md`](docs/plan-phase-1.md) — see
  "Worktree-per-Session", the "SQLite Schema" `worktree` table (which lands in
  milestone 9, not here — M8 uses a minimal precursor like M7 did for resume
  bindings), and Build-Order milestone 8 ("Worktree-per-session: bind/ownership/
  warnings"; *Check:* two sessions, two worktrees, no shared tree).
- Phase scope: [`docs/phases/01-core-local-sessions.md`](docs/phases/01-core-local-sessions.md).
- Reference source (vendored locally): **Kandev**
  `/tmp/kandev/apps/backend/internal/worktree/worktree.go:24-115` (the `Worktree`
  record + non-fatal warning fields: `FetchWarning`, `BaseBranchFallbackWarning`,
  `SetupScriptWarning`), plus `store.go` / `recreator_test.go` in the same dir
  (ownership + reuse + recreate). **herdr** `/tmp/herdr/src/worktree.rs` is a
  second reference. **herdr integration** is not needed this milestone.

---

## Where we are now (done, verified)

Milestones 1–7 are complete (`cargo build`,
`cargo clippy --all-targets --workspace -D warnings`, `cargo test --workspace`
= 244 passed):

- `crates/protocol` — typed control envelopes; session lifecycle + attach types;
  `AgentKind`; `AgentActivity` + `SessionInfo.activity`; `StateSource`;
  `agent_state` event; `session.input`. **M7 added:** `session.report_native_id`
  (`SessionReportNativeIdParams`/`Result`), `integration.install`
  (`IntegrationInstall{Params,Result,Report}`), and `SessionInfo.native_session_id`
  (additive, serde skip-if-None).
- `crates/daemon` (`zagentmeshd`) — Unix-socket server; full `session.*`
  lifecycle; attach bridge; `subscribe` event stream; in-memory `SessionRegistry`
  owning one PTY per session; per-session detection task (state engine).
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon`, `health`/`status`, `session`
  group (`new --agent shell|codex|claude`, `list`, `inspect`, `stop`, `input`),
  `attach <target>`, and **M7's** `integration install [--agent claude|codex]`
  (`inspect` now surfaces `native_session_id` + `resumable`).
- **State engine (M5):** `daemon/src/detect/` (`osc`, `screen`, `manifest`,
  `machine`); per-agent manifests selected by `AgentKind`.
- **Agent adapters (M6):** `daemon/src/agent/` — the `AgentAdapter` trait +
  `codex.rs`/`claude.rs`; input injection honors `InputRules`; resume-argv
  builders (`claude --resume <id>` / `codex resume <id>`).
- **Session-ID hook + resume (M7):** `daemon/src/integration/` installs the
  per-agent `SessionStart` hook (idempotent settings/hooks/config merge; assets
  fire `session.report_native_id`, fire-and-forget, exit 0 on any failure). The
  daemon injects `ZAGENTMESH_ENV/SOCKET_PATH/SESSION_ID/PROTOCOL_VERSION` for
  Codex/Claude; `report_native_id` captures the native id, updates `SessionInfo`,
  and persists a `ResumeBinding`. `daemon/src/store/mod.rs` is a **minimal
  JSON-lines resume-binding store** (the M9 SQLite precursor). On startup the
  daemon `load_and_resume()`s captured sessions; terminal/unresumable bindings
  are pruned so a stopped session never resurrects. `SessionRef { kind: Id|Path }`
  validates id ≤512 / path ≤4096-absolute, no control chars, **no leading `-`**
  (argv-flag-injection guard).
- Stubs (TODO doc-comment only, NOT implemented): `daemon/src/events/mod.rs`
  (append-only event log, milestone 9). `daemon/src/store/mod.rs` now holds the
  minimal resume-binding store with a `TODO(milestone 9)` that SQLite absorbs it.

### Seams milestone 8 builds on (already in place)

- `crates/protocol` — `SessionNewParams` has `agent`, `cwd`, `cols`, `rows`.
  Worktree binding wants a repo + branch + base-branch. M8 adds **optional**
  `repo`/`branch`/`base_branch` (all `#[serde(default, skip_serializing_if)]`)
  to `SessionNewParams`, and `SessionInfo` gains optional `repo`/`branch`/
  `worktree_path` + non-fatal `warnings` (mirror how M7 added `native_session_id`
  additively). The SQLite schema's `session` row already names `repo`,
  `base_branch`, `branch`, `worktree_path`.
- `daemon/src/session/mod.rs` — `create` resolves a `cwd` today. M8 inserts a
  worktree-resolution step: when a repo+branch is requested, bind/create the
  worktree and launch the agent **in the worktree path** instead of the raw cwd.
  `register_pty_session` already takes a resolved `cwd` — feed it the worktree.
- `daemon/src/store/mod.rs` — the minimal-store pattern (load/save a small file
  under the data dir, atomic temp+rename, 0600) is the model for a **minimal
  worktree-binding store** (precursor to the M9 `worktree` table). Do **not**
  build the full SQLite `worktree` table here.
- `crates/cli` — the `session new` command and the `inspect`/`list` formatters
  are where repo/branch/worktree + warnings surface.

---

## Goal of milestone 8

Bind **one git worktree per `(session_id, repository, branch)`** so concurrent
sessions on the same repo never share a working tree. Track `path`, `branch`,
`base_branch`, `status` (`active`/`merged`/`deleted`); check ownership before
reuse or cleanup; and treat fetch / base-branch / setup-script failures as
**non-fatal warnings** (keep the worktree, surface the warning, let the user
decide). **Still local, single-host.**

> **Scope note on persistence (read this).** Like M7's resume-binding store, M8
> ships a **minimal worktree-binding store** (a small file under the data dir:
> `session_id`, `repository`, `branch`, `base_branch`, `path`, `status`,
> timestamps), an explicit **precursor** to the M9 `worktree` table. Do **not**
> build the full SQLite schema or the event log here.

### Definition of done (testable)

1. `SessionNewParams` gains optional `repo`/`branch`/`base_branch`; `SessionInfo`
   gains optional `repo`/`branch`/`worktree_path` + a `warnings: Vec<…>` of
   non-fatal warnings. Additive serde (omitted when absent). Round-trip tests.
2. A worktree module (`daemon/src/worktree/`) that, given a repo + branch +
   base-branch, binds an existing owned worktree or creates a new one under the
   data dir (`worktrees/<session>-<repo>-<branch-slug>/`), running
   `git worktree add`. Ownership check before reuse/cleanup (don't adopt or
   delete a tree this daemon does not own). Branch-slug disambiguation so two
   branches of one `(session, repo)` don't collapse (Kandev `BranchSlug`).
3. Non-fatal warnings: a failed `git fetch` falls back to a local branch with a
   `fetch_warning`; a missing base-branch falls back to the default branch with
   a `base_branch_fallback_warning`; a failing setup script keeps the worktree
   with a `setup_script_warning`. None of these abort session creation.
4. `create_session` launches Codex/Claude in the bound worktree path when a
   repo+branch is requested; a plain `cwd` (no repo) keeps today's behavior.
5. Minimal worktree-binding store under the data dir (load/save, atomic write,
   `TODO(milestone 9)` that SQLite absorbs it). No secrets written.
6. CLI: `session new --repo <path> --branch <name> [--base-branch <name>]`;
   `inspect`/`list` surface the worktree path + branch + any warnings.
7. End-to-end (extend `crates/daemon/tests/health_socket.rs`): create two
   sessions on the same repo with different branches; assert two distinct
   worktree paths (no shared tree) and that each session launched in its own
   worktree. Cover the fetch / base-branch / setup-script warning paths.
8. `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
   `cargo test --workspace` stay clean.

### Explicitly OUT of scope (later milestones — do NOT build here)

- **Full SQLite persistence + `events/` append-only log + migration test** →
  **milestone 9** (absorbs both the M7 resume-binding store and the M8 worktree
  store into `state.db`).
- **`--json` everywhere + error/recovery polish** → milestone 10.
- **Remote transport / NetBird** → Phase 2.

---

## Implementation tasks

1. `crates/protocol` — additive `SessionNewParams`/`SessionInfo` fields +
   round-trip tests (mirror M7's `native_session_id` additive pattern).
2. `crates/daemon/src/worktree/` (new module) — the worktree binder/creator
   (model: Kandev `worktree.go`/`store.go`), ownership check, branch-slug,
   non-fatal warning collection. Unit-test slug, ownership, and warning mapping
   against temp git repos.
3. `crates/daemon/src/session/` + `store/` — resolve the worktree in
   `create_session`, launch in its path; minimal worktree-binding store.
4. `crates/cli` — `session new` repo/branch flags; surface worktree + warnings.

---

## Tests (must pass before done)

- protocol: round-trip for the new optional `SessionNewParams`/`SessionInfo`
  fields (present and absent).
- worktree: slug disambiguation; ownership check rejects a foreign tree; the
  three warning paths (fetch / base-branch / setup-script) keep the worktree.
- session/store: a repo+branch session launches in the worktree; the minimal
  store round-trips; two branches of one repo bind two trees.
- daemon integration (extend `crates/daemon/tests/health_socket.rs`): two
  sessions, two worktrees, no shared tree (the Build-Order checkpoint).
- Keep `cargo build`, `cargo clippy --all-targets --workspace -D warnings`, and
  `cargo test --workspace` clean.

---

## After this milestone

Milestone 9 = **SQLite persistence + append-only event log + `user_version`
1→2 migration test**: absorb the minimal M7 resume-binding store and the M8
worktree store into the `session` + `worktree` tables; `state.db` rebuildable
from sources; the event log is the audit trail. Then milestone 10 = **`--json`
everywhere + error/recovery polish**.

Do not pull milestone 9 SQLite work into this step — keep M8 proven: two
sessions on one repo get two worktrees, ownership is checked before reuse/
cleanup, and fetch/base-branch/setup-script failures are non-fatal warnings.
