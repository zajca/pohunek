# Architecture Review — 2026-07-04

Full-codebase architecture review of the `pohunek` workspace (main @ `06e8ebd`,
v0.15.1). Produced by a six-track parallel review (daemon core, daemon domain
modules, protocol/client/CLI, GUI, support crates, cross-cutting) plus a
synthesis pass. Companion documents:

- `docs/refactor-ms-rust-guidelines-audit.md` — the earlier Rust-guidelines
  audit. Its refactor branch **is merged** (`101043b`); guideline-compliance
  topics are not re-audited here.
- `docs/gui-review.md` — earlier GUI review; its findings are re-verified in
  the GUI section below.

Scope note: the pinned product constraints (single operator, no central server,
PTY/TUI-first, NetBird direct transport, providers shell-out only and never in
the daemon, JSONL store with SQLite deferred, native Iced control-plane GUI
without an embedded terminal) are treated as fixed. Nothing below proposes
changing them.

## Snapshot

~55.7k LOC of Rust across 12 crates:

| Crate | LOC | Files | Notes |
|-------|----:|------:|-------|
| daemon | 25 443 | 41 | `session/mod.rs` 5 867 is the largest file in the repo |
| cli | 8 930 | 19 | |
| gui-core | 6 567 | 6 | `lib.rs` 3 784 |
| gui | 4 870 | 2 | `main.rs` 4 819, zero tests |
| xtask | 2 863 | 8 | |
| protocol | 2 332 | 11 | |
| knowledge | 1 181 | 7 | `protocol` feature gate |
| terminal | 1 051 | 3 | vt100 compositor/screen |
| netbird | 966 | 5 | |
| client | 903 | 3 | |
| hostcheck | 285 | 1 | |
| prompt | 268 | 1 | |

## Synthesis — top refactoring priorities

Overall verdict: the architecture is **sound and faithful to
`docs/architecture.md`** — layering has no cycles or inversions, the daemon
holds its trust boundary (zero provider/token surface), error/secret/panic/
blocking postures are strong, and no P0 was found by any track. The debt is
concentrated in three places: (1) two files that keep regrowing
(`daemon/session/mod.rs`, `gui/main.rs`), (2) a client SDK that stops one
layer too low so both consumers re-implement the typed call layer, and (3)
duplicated cross-crate contracts (paths/env, doctor check list).

**Meta-finding: decompositions don't hold without enforcement.** Two
independent tracks show the same failure mode: `session/mod.rs` was split to
~1 257 production lines a week ago and is back at ~1 836 (+46 %);
`gui/main.rs` was flagged oversized at 2 830 lines in the June GUI review and
is now 4 819. One-off splits are not enough — add a cheap guard (an xtask
check warning on source files whose production span exceeds a threshold) so
regrowth is visible in review.

### Recommended order of work

**Wave 1 — zero-risk quick wins (hours)**
1. Move `session/mod.rs` inline tests (~4 030 lines) to `session/tests.rs`,
   mirroring the worktree pattern (D1).
2. Doc/marker fixes: add `terminal` to the AGENTS.md repo map (S2); fix stale
   "stubs" comments in root `Cargo.toml` and `daemon/src/lib.rs` (S3/T5); add
   the missing `worktree.remove` row to `docs/public-api.md` (C3); delete the
   `__PLACEHOLDER__` no-op in xtask `build_site` (X5).

**Wave 2 — small diffs that remove real runtime risk (days)**
3. Bound worktree git subprocesses with timeouts, `fetch_origin` first — the
   only finding with genuine hang-the-daemon risk (M1).
4. Move hot-path store writes (`persist_resume_binding` and friends) into
   `spawn_blocking`, matching the module's own convention (D3).
5. Stop `apply` auto-vivifying unknown hosts in gui-core (G3).
6. Replace the hand-spliced Codex `config.toml` editing with `toml_edit` or
   fail-closed parsing (M4).

**Wave 3 — the structural refactors this review is about (weeks, ordered)**
7. **Typed SDK call layer in `client`** (C1) built on a
   `Method { NAME, Params, Output }` trait in `protocol` (C2). Absorbs the
   is-local triplication (C5) and request-id inconsistency (C7), deletes ~50
   hand-written decode sites across cli and gui-core, and is the prerequisite
   for the roadmap's SDK-extraction goal.
8. **Shared `pohunek-paths` home** for the socket/runtime/XDG contract and
   `require_env` (T1, T2) — the highest-risk duplication because the layout
   will change pre-1.0.
9. **GUI decomposition**: first split gui-core's `Message` into domain events
   vs UI intents (G1 — the root cause), then the mechanical module cut of
   both giant files (G2), then single selection owner (H2). Consider the
   `control-core` extraction (G5) only after G1 settles the enum.
10. **Daemon registry shape**: group `SessionRegistryInner`'s 17 fields into
    owned sub-structs and relocate agent-reporting/event-log/admin logic out
    of `mod.rs` (D2, D9); split `api/handler.rs` by method domain (D5).
11. Delete the dead adapter resume path; make `base_resume_template` the
    single source of resume-argv shape (M2).

**Wave 4 — consolidation and hygiene (as time allows)**
12. `update_info` persist choke point (D4); `list_raw()` without label
    enrichment (D6); `hostcheck::standard_checks` shared doctor list (X1);
    xtask dedup + split (X2–X4); rename the detect/detect name collision
    (M3) and the `integration` module (M5); assistant.rs unit tests (G4);
    client `handshake()` helper (C4); the remaining P3s.

### Finding index by severity

| Severity | Findings |
|----------|----------|
| P1 | D1, D2, D3*, M1, G1, G2, C1, C2, T1, S1 |
| P2 | D4, D5, D6, M2, M3, M4, G3, G4, G5, G6, C3, C4, T2, T3, X1, X2, X3, X4 |
| P3 | D7, D8, D9, M5, G7, C5, C6, C7, T4, T5, X5–X9, S2, S3 |

*D3 is P1/P2 borderline: mis-scheduled blocking I/O on the request hot path.

## Findings established during the synthesis pass

### S1. `daemon/session/mod.rs` regrew after the split — mostly, but not only, tests (P1)

The merged guideline refactor split `session/mod.rs` from 5 854 down to 3 921
lines (production ≈ 1 257, inline tests from line 1 258). One week of feature
work later (nested active-agent reporting `15c2ed6`, resume bindings `2752fbe`,
attach banner series) it is at **5 867 lines** — production ≈ 1 836 (lines
1–1836), inline tests ≈ 4 030 (from `#[cfg(test)]` at `session/mod.rs:1837`).
So 69 % of the headline number is colocated test code, but the production
surface still grew ~46 % in a week, and new logic keeps landing in `mod.rs`
rather than the extracted `session/{lag,input,attach,hooks,resume,target,detector}.rs`
modules.

Why it hurts: the decomposition does not hold under normal feature velocity,
and the giant inline test module makes the real production surface hard to
review. Fix in two steps: (1) move the test module out to `session/tests.rs`,
mirroring the existing `worktree/tests.rs` pattern — zero behavior risk (this
is also the daemon-core track's top finding, D1 below); (2) re-cut production
boundaries along the observed change axes (active-agent reporting, resume
binding lifecycle, event-log lifecycle) per finding D2, and add a lightweight
guard (e.g. an xtask max-file-size warning) so regrowth is visible in review.

### S2. AGENTS.md repo map is missing the `terminal` crate (P3, docs drift)

`crates/terminal` is a workspace member (root `Cargo.toml`) but the AGENTS.md
"Repository map" table does not list it. AGENTS.md is the canonical agent
guide and promises to be kept current in the same change; the crate table is
the first thing every agent reads. Fix: add a row for `crates/terminal`.

### S3. Root `Cargo.toml` header comment is stale (P3, docs drift)

The workspace manifest still says "Build-Order milestones 1-3 are implemented
in this revision …; later modules are stubs", which described the phase-1
state. The workspace is at v0.15.1 with all major modules implemented. Fix:
replace with a one-line description and pointer to AGENTS.md.

## Daemon core

Scope: `crates/daemon/src/{session,api,store,events,pty}` + `main.rs` wiring.
No P0 flaws found.

### How the pieces actually fit (from the code)

- **Transport (`main.rs`, `api/mod.rs`).** `main::run` binds a `ControlServer`
  (Unix socket) and an optional `RemoteServer` (NetBird TCP), both driven by
  the *same* `serve_connection` (`api/mod.rs:314`), generic over
  `AsyncRead+AsyncWrite`. Each connection is one Tokio task. A line-framed JSON
  request goes to `handler::dispatch_line` (`api/handler.rs:97`), which returns
  `Dispatch::{Reply, Subscribe, Attach}`. `Subscribe` turns the socket into a
  one-way event stream; `Attach` redeems a token and bridges raw PTY bytes
  (`run_attach_connection`, `api/mod.rs:353`).
- **Dispatch (`api/handler.rs`).** `handle_request` (`:144`) is a ~35-arm
  `match` fanning to per-method handlers that call `SessionRegistry`,
  `ProjectManager`, `capabilities`, `assistant`, `integration`, `doctor`.
  Blocking work (`project.*`, `daemon.doctor`, `assistant.materialize`) is
  correctly pushed through `run_blocking`/`spawn_blocking` (`:265`).
- **Registry (`session/*`).** `SessionRegistry` is `Arc<SessionRegistryInner>`
  (`session/mod.rs:250-302`); `inner.sessions: Mutex<HashMap<SessionId,
  SessionEntry>>` is the in-memory source of truth. The impl is split across 8
  files, all `impl SessionRegistry` over the one shared `inner`.
- **Persistence (`store/mod.rs`).** One owner-private JSONL file, three tagged
  record kinds, a single write lock, whole-file temp-write-plus-`rename` per
  mutation (`:676`). `persist_resume_binding` (`session/resume.rs:42`) projects
  live `SessionInfo` + frozen `ResumeSnapshot` into a `ResumeBinding`.
- **Events.** One `broadcast::Sender<Event>` fans to socket subscribers, the
  `EventLog` drain task (`events/mod.rs:147`), and the agent-state hook
  dispatcher (`hooks.rs:212`). PTY *output* is a **separate** per-`PtyHandle`
  broadcast (`pty/mod.rs:136`) that the event log never taps — a clean
  secret/data boundary.
- **PTY (`pty/mod.rs`).** One OS thread per PTY drains output into a ring
  buffer + broadcast under one lock (`:230`); exit via `watch`; resize/kill via
  `spawn_blocking`. **Detector (`session/detector.rs`)**: one task per session
  consumes the PTY output broadcast → `record_activity` → `agent_state` events.

### Findings (most severe first)

**D1 — P1: `session/mod.rs` is 69 % inline test code; production is only
~1 836 lines.** The 5 867-line file splits at `#[cfg(test)] mod tests`
(`session/mod.rs:1837`): production is lines 1–1836, tests ~4 030 lines. The
sibling module already solves this — `worktree/mod.rs` (1 496) keeps its tests
in `worktree/tests.rs` (1 219). *Fix:* move the test module to
`session/tests.rs` (or split by concern), mirroring the worktree pattern. Zero
behavior risk; highest-value, lowest-risk change, and it should precede any
deeper refactor.

**D2 — P1: `SessionRegistryInner` is a genuine god-object (the file split
hides it).** `inner` (`session/mod.rs:255-302`) carries 17 fields spanning 8
unrelated concerns: session map; attach token maps (`pending_attaches`,
`active_attaches`); id allocation; daemon lifecycle (`daemon_shutdown_started`,
`daemon_instance_id`); persistence (`store`, `persist_lock`); worktree binding;
project management; event-log task lifecycle; hook-dispatcher lifecycle. The
per-file split is cosmetic — every method reaches into the same flat `inner`,
so nothing is actually encapsulated. *Fix (after D1):* group the flat fields
into cohesive owned sub-structs, e.g. `AttachRegistry { pending, active,
next_stream_id }` (natural home in `attach.rs`), `BackgroundTasks
{ event_log_*, agent_state_hook_* }`, `Persistence { store, persist_lock }`.
The public `SessionRegistry` facade stays; only `inner`'s shape changes.
Production responsibilities to relocate out of `mod.rs`: agent reporting
`report_native_id`/`report_agent`/`release_agent` (`:919-1175`, ~260 lines) →
new `agent_report.rs`; event-log lifecycle `spawn_event_log`/
`shutdown_event_log` (`:636-676`) → `eventlog.rs`; project/worktree admin
`remove_project`/`remove_worktree` (`:461-622`) → `admin.rs`; core
CRUD + `record_exit` stays in `mod.rs`.

**D3 — P1/P2: blocking store I/O runs directly on the async runtime,
inconsistently with the rest of the module.** Everywhere else blocking
git/store work is offloaded via `spawn_blocking` (`resolve_project`,
`bind_worktree`, `remove_project`, `remove_worktree`, `enrich_project_labels`,
`PtyHandle::spawn`). But `persist_resume_binding` calls `store.record_resume` /
`store.remove_resume` **synchronously** on the executor thread while holding
`persist_lock` (`session/resume.rs:65-67`); `restore_worktree_metadata` (`:362`)
and `load_and_resume` (`:88`) do the same. Each does `fs::read_to_string` +
serialize + temp-write + `rename` of the whole store file — and it is on the
**hot path**: `resize`, `set_metadata`, `rename`, `report_native_id`,
`record_exit`, and `remove` all call `persist_resume_binding` for a captured
session (`session/mod.rs:1007,1265,1302,1363,1577,1444`), so a captured
session's every resize blocks a Tokio worker on file I/O. *Fix:* wrap the store
call in `spawn_blocking` (owned `Arc`, hold `persist_lock` across the awaited
join), matching the module's own convention.

**D4 — P2: in-memory `SessionInfo` and the persisted `ResumeBinding` are
re-synced by hand with no choke point.** Every mutating method must *remember*
to call `persist_resume_binding` after editing `entry.info` (and guard on
`has_native`): `resize`/`set_metadata`/`rename` do (`:1362,1264,1301`),
`report_agent`/`release_agent` deliberately do not (active-agent state is not
persisted). A future mutating method that forgets the call silently breaks
restart-resume, and nothing in the type system flags it. *Fix:* funnel
`entry.info` mutations through one `update_info(id, f)` primitive that
centralizes the persist decision.

**D5 — P2: `api/handler.rs` (1 160 lines) mixes seven method domains in one
file.** `handle_request` and its ~30 `handle_*` fns cover `session.*`,
`project.*`, `worktree.*`, `host.*`, `daemon.*`, `assistant.*`,
`integration.*` in a single module (`api/handler.rs:153-695`). *Fix:* keep
`handle_request` as the router; split handlers into
`handler/{session,project,worktree,host,daemon}.rs`; shared helpers
(`parse_params`, `ok_value`, `run_blocking`) → `handler/util.rs`. Mechanical,
low-risk.

**D6 — P2: `enrich_project_labels` does a store read on every `list()`,
including where labels are unused.** `list()` (`session/mod.rs:1181`) calls
`enrich_project_labels` → `spawn_blocking(label_map)` (`:1205`) whenever any
session has a `project_id`. `list()` is also invoked purely to gather live
worktree paths inside `remove_project` (`:481`) and `remove_worktree` (`:584`),
where labels are discarded. For a launcher/GUI polling `session.list`, this is
a file read per poll. *Fix:* add a `list_raw()` (no enrichment) for internal
callers; consider a short-TTL label cache for the polled path.

**D7 — P3: version negotiation runs twice on the socket path.**
`dispatch_line` negotiates (`api/handler.rs:125`) then calls `handle_request`,
which negotiates again (`:149`). `handle_request` needs its own check (tests
call it directly), but the socket path double-checks. Drop the `dispatch_line`
copy or document why both exist.

**D8 — P3: local and remote transports get independent
`DiscoveryCache`/`HealthInfo`.** `main.rs` builds two `DaemonState`s
(`main.rs:105,117`), each with its own `DiscoveryCache` (`api/handler.rs:61`).
A `host.discover --force` on one transport doesn't warm the other's cache. The
session registry is correctly shared. *Fix:* share one `Arc<DiscoveryCache>`
if remote discovery matters.

**D9 — P3: two methods already carry `#[expect(clippy::too_many_lines)]`
"tracked for session module decomposition":** `create` (`session/mod.rs:687`)
and `resume_binding` (`session/resume.rs:200`). They are the orchestration
seams the D2 decomposition should target — `create`'s inline launch-then-
rollback block (`:732-816`) reads as an extractable "launch attempt with
worktree compensation" unit.

### Already good, keep

- **Lock discipline:** the async `sessions` mutex is never held across PTY
  spawn, store I/O, or `.emit`; mutating methods snapshot into a local, drop
  the guard, then do I/O and emit (`resize` `:1335-1354`, `record_exit`
  `:1502-1559`). Only cross-lock order is `persist_lock` → `sessions` inside
  `persist_resume_binding`, and callers always release `sessions` first — no
  inversion.
- **Attach handoff race:** `PtyHandle::attach_snapshot_and_subscribe`
  (`pty/mod.rs:301`) snapshots history and subscribes under the same lock the
  reader thread pushes+broadcasts under — exactly-once replay; reused for the
  initial-input readiness gate (`session/mod.rs:870`).
- **Store atomicity:** single write lock + whole-file temp+`rename`;
  `mutate_project` (`store/mod.rs:544`) closes the read-modify-write race.
  Corrupt lines are skipped and logged, not fatal (`:666`).
- **Supervision:** per-connection tasks isolate panicking handlers; transient
  `accept` errors are logged and the loop continues (`api/mod.rs:164`);
  event-log and hook dispatchers have bounded shutdown-flush semantics.
- **Self-feeding attach guard** scoped by `daemon_instance_id` (session id AND
  instance id must match) so a stale origin can't false-reject across daemon
  restarts (`session/attach.rs:34-67`).

## Daemon domain modules

Scope: `crates/daemon/src/{worktree,project,detect,agent,integration}`.
Overall this subsystem is well-built and faithful to `docs/architecture.md` /
`docs/design/projects.md`: no P0 flaws.

### Responsibilities and dependency map (from the code)

- **`detect/`** — the *agent-activity* state engine (not git). Pure, sync,
  PTY-free: `Detector` (`detect/mod.rs:121`) drives `osc` (OSC 0/2/9 parser),
  the `pohunek_terminal` VT `ScreenTracker`, `manifest`
  (schema + matcher + parser + error), and the debounced
  `machine::StateMachine`. Everything is fed `(now: Instant, bytes)` — zero
  I/O. Consumed by `session` (owns a `Detector`) and `agent` (borrows the
  `Manifest` type).
- **`agent/`** — adapter trait + built-in adapters (`claude`/`codex`/`shell`),
  `SessionRef` validation + resume-argv builders (`agent/mod.rs`), and the
  host-profile registry `profile.rs` (fail-closed, owner-gated
  `agents/*.toml`). Depends on `detect::Manifest`, `pty::PtyCommand`,
  `project::config::validate_name`, `protocol`.
- **`project/`** — three concerns: `detect.rs` (pure git-repo *identity*
  detection with its own timeout-bounded git executor),
  `mod.rs::ProjectManager` (store glue: upsert/resolve/list/show), and
  `config.rs::ProjectConfigResolver` (layered `.pohunek/`
  prompts/templates/actions with its own charset+containment security).
- **`worktree/`** — `WorktreeManager`: git-worktree mechanics,
  implicit-ownership gate (binding = proof), lifecycle hooks, and *its own*
  git executor. Owns general path/URL utils reused by `project`.
- **`integration/`** — the Claude/Codex **`SessionStart` hook installer**
  (writes hook scripts, merges `settings.json`/`hooks.json`/`config.toml`).
  Not a Linear/GitHub provider — it does **not** violate the "no providers in
  the daemon" rule; the hook reports to the daemon socket for native-resume
  binding, so it legitimately belongs host-side.

Notable coupling: `agent::profile` reaches into `project::config::validate_name`
(`agent/profile.rs:24`); `project::{detect,mod}` reach into `worktree` for
`canonical_or_original`/`redact_url_credentials` (`project/detect.rs:32`) —
general utilities that aren't really worktree-specific.

### Findings (most severe first)

**M1 — P1: worktree git subprocesses are unbounded, unlike detection.**
Detection bounds every git call (`project/detect.rs:39` 5 s cap, `run_bounded`
at `:264` with a concurrent reader thread + hard kill). Worktree git goes
through `worktree/mod.rs:1381 run_command`, a plain `.output()` with **no
timeout** — including the network-facing `fetch_origin`
(`worktree/mod.rs:1001`). `bind` runs in `spawn_blocking`
(`session/target.rs:345`), so a hung fetch (slow/dead remote) can stall
`session.new` indefinitely and tie up a blocking thread — exactly the failure
detection was engineered to avoid. `docs/design/projects.md:97` explicitly
requires each git call to be "bounded by a short timeout". *Fix:* route
worktree git through a bounded executor (the poll/deadline pattern already
exists in `wait_with_timeout`, `worktree/mod.rs:1322`); prioritize
`fetch_origin`.

**M2 — P2: the adapter-based resume path is dead in production, kept green
only by tests.** The live resume path is `resume_pty_command_from_template`
driven by the frozen `ResumeTemplate` snapshot (`session/resume.rs:298`). The
older `AgentAdapter::resume()` (`agent/mod.rs:249`), `resume_pty_command`
(`agent/mod.rs:311`), `AgentCommand`/`resume_command` (`agent/mod.rs:164,341`)
have **no production caller** (only tests at `agent/mod.rs:703,730`). Worse,
`base_resume_template` (`agent/mod.rs:224`) re-encodes the same "Claude=flag,
Codex=subcommand" fact the adapters' `resume()` hardcodes, and the test
`base_resume_template_matches_native_adapter_modes` (`agent/mod.rs:736`)
exists solely to keep the two copies in sync. *Fix:* delete the adapter
`resume()` + `resume_pty_command` + `AgentCommand`; make
`base_resume_template` the single source of resume-argv shape (adapters keep
launch/input/manifest only). Pre-1.0, no back-compat cost.

**M3 — P2: two unrelated subsystems are both named "detect".** `crate::detect`
is the terminal *activity* engine; `crate::project::detect` with its
`detect()` fn (`project/detect.rs:106`) is git-repo *identity* detection. Zero
shared code or concepts — a pure name collision that forces readers to track
which "detect" each `use` refers to. *Fix:* rename the activity engine to
`activity` (or `state_detect`), or `project::detect` → `project::repo`.
Related placement quibble: the bounded git runner lives in `detect.rs` as
`pub(crate) git` but is reused by `project show` (`project/mod.rs:374`) — it
is a shared git util, not a detection detail.

**M4 — P2: the Codex installer edits `config.toml` by hand-splicing text
lines, not by parsing TOML.** `enable_codex_hooks_feature`
(`integration/mod.rs:482`) and `ensure_codex_hook_trust_state` (`:520`)
scan/insert lines using an ad-hoc `toml_table_header`/`is_toml_key` recognizer
(`:591,609`). Fragile against valid-but-unexpected shapes: dotted-key tables
(`features.hooks = true`), inline tables, header-with-trailing-comment — a
misparse silently writes a broken or duplicate key into the operator's real
Codex config. *Fix:* edit a format-preserving `toml_edit::Document`, or fail
closed on shapes the line-scanner can't safely handle.

**M5 — P3: smaller structural items.**
- The module name **`integration`** invites confusion with the explicitly
  forbidden provider integrations; it is the agent hook installer. Rename to
  `agent::hook_install` (it arguably belongs under `agent/`).
- **Argv/name guards are scattered across four validators** with subtly
  different rules: `SessionRef` (`agent/mod.rs:86`), `validate_git_ref_arg`
  (`worktree/mod.rs:848`), `validate_name` (`project/config.rs:295`),
  `validate_project_name` (`project/mod.rs:451`); the leading-dash guard is
  duplicated in the first two. Rules genuinely differ per trust boundary, but
  a reviewer can't see all injection guards in one place — gather them into
  one `validate` module sharing the leading-dash primitive.
- **`project` bundles three concerns**; `config.rs` (972 LOC, its own security
  model) is independent enough to lift to a top-level module, leaving
  `project` = detection + records.
- **Duplicate porcelain parsers:** `project/detect.rs:206 main_worktree` and
  `project/mod.rs:383 parse_worktrees_porcelain` both scan
  `git worktree list --porcelain -z`; the former could reuse the fuller
  parser's first entry.
- **General utilities living in `worktree`:** `canonical_or_original` and
  `redact_url_credentials` (`worktree/mod.rs:941,1442`) are consumed by
  `project`; move to a shared `util`/`path` module.

### Already good, keep

- **Detection state machine is textbook-testable.** `machine::StateMachine`
  (`detect/machine.rs:97`) and `Detector` are pure over `Instant`/bytes; the
  debounce/flicker logic has dense unit tests with no PTY. Sharing the VT
  model via `pohunek_terminal` (`detect/mod.rs:22`) avoids drift with the CLI.
- **Manifest schema validates and fails fast.** Typed `ManifestError`s,
  `deny_unknown_fields`, complexity budget (`detect/manifest/mod.rs:26`);
  embedded manifests `.expect`-parse at startup as trusted constants while
  host-profile manifests use the non-panicking path and fail the profile
  closed (`agent/profile.rs:408`).
- **Security posture is consistently fail-closed.** Profile tree owner-gated
  at boot with symlink containment (`agent/profile.rs:131,261`); config
  resolver's charset + canonicalize-and-contain guards with non-leaking errors
  (`project/config.rs:335,381`); argv-flag-injection guards on every
  socket-supplied value fed positionally to git/agents (`agent/mod.rs:110`,
  `worktree/mod.rs:848`); `--end-of-options` defense-in-depth
  (`worktree/mod.rs:1006`); credential redaction before any git error is
  persisted (`worktree/mod.rs:1442`); hooks run env-cleared with a non-secret
  allowlist in their own process group (`worktree/mod.rs:1197`).
- **Worktree ownership model is disciplined.** Binding = ownership proof;
  reuse/recreate/refuse-foreign is explicit (`worktree/mod.rs:233`); cleanup
  and `--prune-worktrees` only touch recorded bindings and skip live sessions
  (`worktree/mod.rs:525`); a failed binding-persist rolls the checkout back
  (`worktree/mod.rs:362`).
- **`ProjectManager` upserts are race-safe** — read-modify-write under the
  store lock preserving operator-owned fields (`project/mod.rs:107`).

## Protocol, client SDK, CLI

Scope: `crates/protocol`, `crates/client`, `crates/cli`, plus how gui-core
consumes the client.

### How a request actually flows

- **Control request (CLI):** `run(cli)` (`cli/src/lib.rs:677`) dispatches one
  arm per subcommand to a command fn. The command builds a `Request` via
  `commands::request_with_params` (`cli/src/commands/mod.rs:81`; unique id
  `cli-<method>-<token>-<seq>`), connects through the thin CLI wrapper onto
  `pohunek_client::Client`, calls `client.request(&req)`, and decodes the raw
  `ok` payload (`serde_json::Value`) itself before rendering.
- **Transport (SDK):** `Client::connect` (`client/src/transport.rs:104`) picks
  Unix vs TCP via `is_local_host`; remote resolves the NetBird address in a
  `spawn_blocking` (`transport.rs:486`). `Conn::exchange` (`transport.rs:254`)
  frames one line with `LinesCodec` (1 MiB cap), verifies the echoed `id`, and
  returns `ok` or maps `err`. On timeout or id-mismatch it **poisons** the
  connection so a desynced socket is never reused (`transport.rs:216-234,
  277-284`).
- **Daemon contract surface:** `dispatch` is a flat 36-arm match over
  `method::*` string constants; each arm re-parses `request.params` into a
  typed struct.
- **Attach diverges:** `session.attach` on the control connection returns a
  one-shot `stream_id` (`cli/src/commands/attach.rs:325`); the CLI opens a
  second raw connection and writes an `{"attach":stream_id}` prelude
  (`client/src/transport.rs:363-445`), after which bytes are opaque
  bidirectional PTY. `session.resize`/`session.detach` still go over a control
  connection (`attach.rs:522-540`). gui-core does not implement attach
  (consistent with the terminal-less control-plane direction).

### Findings (most severe first)

**C1 — P1: `client` is a transport, not an SDK; the typed method layer is
duplicated in both consumers.** `client` exports only
`Client::request(&Request) -> Value` plus raw/attach helpers
(`client/src/lib.rs:10-15`). Every "call method X, decode result Y" pair is
reimplemented twice: the CLI inlines `let v = client.request(&req).await?;
let r: SessionNewResult = serde_json::from_value(v)?;` in each command
(`cli/src/commands/session.rs:235-236`, ~29 such sites), while gui-core has a
generic `request_json<T>` (`gui-core/src/lib.rs:2652`) wrapped by ~20
per-method public fns (`gui-core/src/lib.rs:2007-2400`). This is the concrete
form of the prior audit's "SDK Extraction Needs Protocol and Client API
Polish" — and `docs/public-api.md:328` already tells Rust clients to prefer
the SDK "rather than hand-writing protocol framing"; today they still
hand-write the typed layer. *Fix:* move the typed wrappers into `client`
(free fns or small `SessionApi`/`ProjectApi`) returning typed results; both
consumers collapse to one call and the untyped `Value` edge stops leaking.

**C2 — P1: no type-level pairing of method ↔ params ↔ result.** Methods are
bare `&str` constants (`protocol/src/lib.rs:85-161`); params/results relate
only by naming convention and the public-api.md table (confirmed: no
`type Params`/`type Result` anywhere in `crates/protocol`). Nothing stops a
caller pairing `SESSION_STOP` with `SessionResizeParams`; discoverability is
by grep. *Fix:* `trait Method { const NAME: &str; type Params; type Output; }`
implemented per method — makes the daemon's 36-arm match and both clients
type-checked and discoverable, and is the natural foundation for C1
(`client.call::<SessionNew>(params)`).

**C3 — P2: public-api.md drift — `worktree.remove` is undocumented.** It is a
live dispatched method (`daemon/src/api/handler.rs:187`) and gui-core calls it
(`gui-core/src/lib.rs:2311`), but it is absent from the Public Methods table
(`docs/public-api.md:141-148`). Per AGENTS.md/CLAUDE.md the doc must reflect
every protocol method. *Fix:* add the
`worktree.remove | WorktreeRemoveParams | WorktreeRemoveResult` row.

**C4 — P2: version negotiation is daemon-only; the client half is effectively
dead.** `negotiate` is re-exported to clients (`protocol/src/lib.rs:73`) but
the SDK never calls it — only the daemon does (`handler.rs:125,149`) and its
discovery path. The client relies on the daemon rejecting a skew with
`daemon/version_mismatch`; `docs/public-api.md:24` advises "call
`daemon.health` after connecting," but nothing in `client` supports or
enforces it. *Fix:* add a `Client::handshake()`/`daemon_version()` helper that
performs the health probe and surfaces the negotiated version once.

**C5 — P3: the "is this local?" predicate is triplicated.** Identical
`host.is_empty() || host == "local"` logic in `client/src/transport.rs:482`
(private, so unreusable), `cli/src/target.rs:28`, and inlined again in
`cli/src/commands/assistant/bootstrap.rs:52`; `LOCAL_HOST` is a separate const
in both client and cli. *Fix:* export `is_local_host`/`LOCAL_HOST` from
`client`.

**C6 — P3: two commands bypass the single `--json` success sink.**
`render_json` (`cli/src/commands/mod.rs:34`) exists so every command
serializes identically; `health.rs:33` and `doctor.rs:70` hand-roll
`println!("{}", serde_json::to_string_pretty(...)?)`. Route them through
`render_json`.

**C7 — P3: request-id conventions differ across consumers.** The CLI mints
per-call unique ids (`mod.rs:64`); gui-core uses static literals like
`"gui-session-new"` (`gui-core/src/lib.rs:2023`) that alias every repeat call
in logs. Folding the call layer into `client` (C1) unifies id generation for
free.

### Already good, keep

- **Error taxonomy layering is clean and lossless.** `ProtocolError` (wire) →
  `ClientError` (adds host/transport context) → `CliError`/`CoreError`, each
  with `to_protocol_error()`; daemon errors pass through with
  `class`/`code`/`recover` intact (`client/src/error.rs:80-131`,
  `cli/src/error.rs:123-205`).
- **Forward-compat envelope discipline.** Opaque `Value` payloads at the
  envelope layer, additive fields, `#[serde(default, skip_serializing_if)]`,
  untagged `ok`/`err` `Response`; the additive contract is documented and
  tested (`protocol/src/session.rs:757-792`).
- **Protocol is already split by domain into files**
  (`session`/`project`/`assistant`/`discovery`/`doctor`/`capabilities`/
  `integration`); only the `method`/`event` constant modules and the flat
  crate-root re-export remain single namespaces.
- **clap usage errors are funneled into the same `{class,code,msg,recover}`
  JSON envelope** as runtime errors (`cli/src/error.rs:302-332`).
- **Connection poisoning after timeout/id-mismatch** and the **attach
  self-feedback guard** via `origin_session_id`+`origin_daemon_id`
  (`protocol/src/session.rs:163-191`) are well-reasoned safety mechanisms.
- **Layering respected:** gui-core depends only on client + protocol, never
  on cli.

Through-line: the wire contract and error model are in good shape; the gap is
that `client` stops one layer too low, so the typed request/response surface
(and its id/version/local-host helpers) is reinvented in `cli` and `gui-core`.
C1 + C2 are the highest-leverage changes and absorb C5/C7.

## GUI (gui + gui-core)

Baseline: `docs/gui-review.md` (2026-06-29), re-verified against current main.
Headline: the security item (M5) and most low-priority items were fixed, but
the two structural findings (H1 module size, H2 dual selection) are **still
open — and H1 regressed**: `main.rs` grew 2 830 → **4 819 LOC**, `lib.rs`
3 115 → **3 784**, plus a new, thinly-tested `assistant.rs` (864 LOC).

### Status of prior gui-review.md findings

| ID | Finding | Status | Evidence |
|----|---------|--------|----------|
| H1 | Oversized `main.rs`/`lib.rs` | **Open — worse** | 4 819 / 3 784 LOC; still single files |
| H2 | Dual source of truth for `selection` | **Open** | two `selected_github_scope`: `lib.rs:1070` (over `workspace.selection`) vs `main.rs:1776` (over `ui_state.selection`); hand-sync at `main.rs:557,589,248` |
| H3 | Inconsistent error routing | **Partially fixed** | provider ops now route to host-scoped `ProviderOperationFailed` via `push_provider_task_result` (`main.rs:982`); session/project CRUD still land in global `app.status` (`main.rs:958`) |
| M1 | Duplicated `*_task` builders | **Open** | ~13 builders + ~20 identical `match` arms in `update` (`main.rs:660-750`) |
| M2 | Doubled `_with_options` API | **Open** | every SDK fn still doubled (`lib.rs:2079-2390`); no-options twins have no production callers, only `tests/loopback.rs` |
| M3 | Hardcoded 80×24 | **Fixed** | `DEFAULT_TERMINAL_COLS/ROWS` (`main.rs:46-47`); remaining literals are test helpers |
| M4 | No tracing | **Partially fixed** | gui-core emits ~12 structured `tracing` events (`lib.rs:1779-2640`); **`gui/main.rs` still has zero** (one boot `eprintln`, `main.rs:66`) |
| M5 | `sh -c` injection | **Fixed** | `shell_escape` allowlist + single-quote escaping (`lib.rs:3039`); `render_attach_command` escapes `{bin}/{host}/{id}` (`lib.rs:3029`) |
| L1 | `unreachable!` in provider map | **Fixed** | returns `Err(CoreError::UnsupportedPromptProvider)` (`lib.rs:2526`) |
| L3 | Duplicated branch-field contract | **Fixed** | consts + single consumer `branch_from_context` (`lib.rs:2532`) |
| L7 | `notification_command` default | **Fixed** | `DEFAULT_NOTIFICATION_COMMAND` const (`main.rs:49`) |
| L2/L4/L5/L6 | window dims / per-request client / no jitter / lint set | **Open** | `request_host_json` still connects a fresh client per command (`lib.rs:2610`); reconcile still full-snapshot-reloads per tick |

### New findings (most severe first)

**G1 — P1: `gui-core::Message` conflates UI intents with I/O results, and the
shell pushes GUI intents into it.** `Message` (`lib.rs:595-759`, ~40 variants)
mixes async daemon results (`HostSnapshotLoaded`, `*Loaded`, `*Completed`,
`HostEvent`) with pure UI-state intents (`ProviderPanelSelected`,
`LinearProviderFilterSelected`, `*Selected`). The shell constructs the latter
and calls `app.workspace.apply(CoreMessage::…)` straight from `update`
(`main.rs:755,800,821,686`), so the core's public vocabulary is half
domain-events, half view-events, and `apply()` (~600 lines,
`lib.rs:1100-1700`) is both a reducer for daemon data and a UI-intent handler.
This is the root cause that makes both giant files hard to split. *Fix:*
split into (a) `DomainEvent` — I/O results the core reduces — and (b) UI
intents that stay in the shell and mutate `ProviderState` through small typed
methods (`set_active_panel`, `select_linear_filter`), not through the
wire-message enum.

**G2 — P1: H1 decomposition (regressed, now urgent).** Concrete cut, pure code
movement:
- `crates/gui/`: `message.rs` (enum, ~426-509); `update.rs` (dispatch,
  521-980); `command.rs` (`*_task` builders + `push_provider_task_result`,
  982-2043); `selection.rs` (~10 `selected_*` helpers, 1660-1980);
  `view/{mod,tree,detail,modals,provider,pills}.rs` (tree 2239-2530, detail
  2532-2760, modals 2975-3220, provider 3253-3760); `config.rs`
  (`AppConfig`/`RawConfig`/`ConfigError`, 3938-4400); `attach.rs`
  (`ShellAttachSpawner`, 3864-3923). `main.rs` keeps boot + `subscription` +
  `theme`.
- `crates/gui-core/`: `message.rs` (post-G1 enum); `state.rs`
  (`Workspace`/`HostView`/`apply`/`AgentMonitor`, 965-1850); `sdk.rs`
  (`request_*`/`*_with_options`, 1991-2660); `connection.rs`
  (`host_connection_stream`/`Backoff`/`reconcile_interval`, 2666-2900);
  `link.rs` (session-link metadata + launch flows, 279-447 + 2440-2551);
  `ui_state.rs` (`UiState`/`Selection`/`TreeNodeId`/`WindowSize`, 763-945);
  `attach.rs` (`render_attach_command`/`shell_escape`, 3006-3120).

**G3 — P2: `apply` auto-vivifies hosts on any result.** ~30 arms do
`self.hosts.entry(host_id).or_insert_with(HostView::connecting)`
(`lib.rs:1213,1220,1252,1289,1350`). A late/stray result for a host that was
never discovered — or was removed — silently resurrects it as a phantom
"Connecting" node. Scope/request-id guards protect provider data but not host
existence. *Fix:* drop-with-trace results addressed to an unknown host
(`trace_ignored_provider_result` already exists); reserve insertion for the
connect path.

**G4 — P2: `assistant.rs` (864 LOC) is a new I/O-heavy state machine that is
barely tested.** `tests/assistant.rs` is 56 lines / 4 tests; zero inline unit
tests. Yet it does local/remote/degraded bundle materialization
(`materialize_local/remote/degraded`, 437-519), snapshot collection + prompt
composition (`collect_snapshot`, `compose`, 629-790), and — security-relevant
— `preflight_read_access`/`check_readable` (562-600) gating which files the
assistant can read. Compose logic and the read-access preflight are pure and
need unit tests (denied path, symlink escape, missing snapshot).

**G5 — P2: gui-core is now the shared control-plane domain crate but
named/shaped as GUI-only.** `crates/cli/src/commands/assistant/mod.rs:21`
depends on `pohunek_gui_core::assistant` (good reuse), yet the crate also owns
persisted view-model types (`UiState`, `TreeNodeId`, `WindowSize`,
`Selection`, `Toast`, `ProviderPanel`) that the CLI links transitively.
*Fix:* extract the shared domain/SDK/assistant surface into a `control-core`
crate, leaving the view-model in `gui-core`; at minimum stop widening the
enum with view intents (G1) so the CLI's dependency stays domain-only.

**G6 — P2: `_with_options` doubling isn't even a uniform convention.**
`lib.rs` doubles every SDK call (M2), but `assistant.rs` exposes only
`*_with_options` variants with no non-options twin (`assistant.rs:237,319,352`)
— the suffix means "the real one" in one module and "test-convenience alias
exists" in another. *Fix:* drop the no-options twins in `lib.rs` (tests pass
`ConnectionOptions::default()`); the suffix disappears everywhere.

**G7 — P3: `update` arm boilerplate (M1) leaked into the dispatcher.** ~20
arms are the identical `match some_task(app) { Ok(task) => tasks.push(task),
Err(err) => app.status = Some(err) }` (`main.rs:660-750`). A
`fn run_task(app, tasks, Result<Task, String>)` collapses these and makes the
H3 routing policy a one-line change.

### Already good, keep

- **Headless/view split is real and holds**: gui-core has no Iced dep;
  `apply`/`update` centralize transitions; state machine well exercised for
  stale-response/scope/backoff (`tests/loopback.rs`, 58 KB).
- **Provider ports, not copy-paste**: `GhRunner` (github), `TokenSource` +
  `GraphqlTransport` (linear) are clean trait boundaries with prod + test
  impls; the two clients are genuinely different backends.
- **Secret hygiene**: Linear token read per-call via `KeyringTokenSource`,
  never persisted; redacting hand-written `Debug`; `gh` stderr scrubbed.
- **Monotonic `ProviderRequestId` + scope guards** with structured
  `trace_ignored_*` on rejection.
- **CLI reuses `gui-core::assistant`** rather than reimplementing.
- **Config fails fast** with typed `ConfigError`; magic dims are named
  constants.

Suggested order within the GUI: G1 message-enum split (unlocks clean H1
decomposition) → G2 module movement → H2 single selection owner → G3
auto-vivify guard + G4 assistant tests → M1/M2/M4-shell cleanup.

## Support crates (terminal, netbird, hostcheck, prompt, knowledge, xtask)

These are the healthiest crates in the workspace — small, well-documented,
heavily tested. Findings are maintainability/duplication issues, not
correctness or security defects. No P0/P1. Verified: `pohunek-knowledge`
builds cleanly under `--no-default-features`, default, and
`--features protocol`.

### Role + actual-consumers map

- **terminal** (1 051 LOC) — vt100-backed screen model. `ScreenTracker` (grid
  scraping for activity detection) consumed by the daemon
  (`detect/mod.rs:24` re-exports it); `Compositor` (banner-over-agent-grid
  re-render for attach) consumed by the CLI (`commands/attach.rs:13`).
  Correctly a shared crate — it sits between daemon and cli. **The suspected
  vt100 duplication with the daemon does not exist**: the daemon has no direct
  `vt100` dep and consumes `pohunek_terminal`; this crate IS the de-dup
  boundary.
- **netbird** (966 LOC) — `status --json` parsing, host→IP resolution,
  fail-closed bind-address validation, port resolution. Consumers: cli,
  daemon, client, hostcheck. The "0 test files" impression is a false alarm:
  31 inline `#[cfg(test)]` tests across all 5 modules, and the 6 promised
  fixtures in `tests/fixtures/` are used via `include_str!`
  (`status.rs:286-293`, `host.rs:99`).
- **hostcheck** (285 LOC) — host-probe functions returning
  `protocol::DoctorCheck`. Consumers: cli (`commands/doctor.rs`) and daemon
  (`doctor.rs`). All 7 public probes used.
- **prompt** (268 LOC) — provider prompt template rendering. Consumers: cli,
  gui-core.
- **knowledge** (1 181 LOC) — assistant knowledge-bundle primitives.
  Consumers: daemon + gui-core (both `features=["protocol"]`), xtask
  (without). Feature-gate hygiene clean.
- **xtask** (2 863 LOC) — workspace automation. `docs check` + `docs site`
  run in CI; `eval` is a manual-only release gate.

### Findings (most severe first)

**X1 — P2: hostcheck's check-list composition is duplicated across its two
consumers.** `daemon/src/doctor.rs:14-42` and `cli/src/commands/doctor.rs:24-67`
assemble the same 15 checks in the same order, including an identical
hardcoded `schema_version` `Warn` placeholder — directly against hostcheck's
own doc intent ("the probe logic is identical, so it lives here once"). The
two lists will drift silently. *Fix:* add
`hostcheck::standard_checks(inputs) -> Vec<DoctorCheck>` (taking the few paths
as params); both call sites use it.

**X2 — P2: xtask generators duplicate `write_concept_file` and the
frontmatter template 4×.** Defined identically in `generators/config.rs:73`,
`generators/protocol.rs:160`, `generators/setup_assets.rs:57`,
`generators/cli.rs:428`; the YAML frontmatter block is copy-pasted across all
four. *Fix:* hoist `write_concept_file` into the module root and add a shared
`frontmatter(...)` builder.

**X3 — P2: clap command-validation logic duplicated between eval and
docs-check.** `eval.rs:148-204` (`check_commands_inner`) and `lib.rs:621-651`
(`check_runbook_commands`) both implement "tokenize a `pohunek …` string, run
`cli::command().try_get_matches_from`, treat `DisplayHelp`/`DisplayVersion` as
success, skip `<…>` placeholders." This is the actual contract for detecting
hallucinated/broken commands — it must not diverge between the manual eval
and the CI drift check. *Fix:* extract one
`fn parse_pohunek_command(cmd: &str) -> Result<(), String>`.

**X4 — P2: xtask `lib.rs` (1 050 LOC) mixes four responsibilities.** Command
dispatch (`run`), bundle building (`build_docs`), the entire static-site
renderer (~190 LOC, lines 182-376), and all six drift checks (~380 LOC, lines
394-778) in one file. *Fix:* move site rendering to `site.rs` and checks to
`checks.rs`; pure code motion.

**X5 — P3: dead no-op in `build_site`.** `lib.rs:248-249` inserts a
`__PLACEHOLDER__` sentinel after every `href="` then immediately strips it —
provably a no-op; the real link rewriting is `replace_md_links_in_html` on
line 251. Delete both lines.

**X6 — P3: prompt `joined_field_name` is fragile slice-pattern matching.**
`prompt/src/lib.rs:164-171` maps a `&'static [&'static str]` back to an error
label by matching exact slice literals, falling through to `"unknown"` — a
new field-name set silently degrades the error message. *Fix:* carry the
label alongside the names (`Field { names, label, required }`).

**X7 — P3: xtask hand-rolls arg dispatch despite depending on clap.**
`lib.rs:780-871` manually parses `eval` / `docs <action>` while clap is
already a dependency. A tiny clap subcommand enum would be more consistent
and give free `--help`.

**X8 — P3: netbird `remote_port` reports a config typo as
`StateUnavailable`.** `port.rs:38-40` returns `StateUnavailable` for an
unparseable `POHUNEK_REMOTE_PORT`, conflating "NetBird state unreachable"
with "operator config error." *Fix:* add an `InvalidConfig` variant. The
fail-loud behavior itself is correct.

**X9 — P3: terminal exposes `visible_lines`/`slice_columns` as `pub` with no
external consumer.** Both used only internally. Demote to `pub(crate)` unless
intended as SDK surface.

### Already good, keep

- **netbird is the model crate**: dual-shape defensive parsing with
  `#[serde(default)]` + untagged `PeersRepr`, fail-closed IP gating applied
  in resolver, bind validator, and raw-IP path alike, bounded error details,
  char-boundary-safe clamping, compile-time port invariants, and the
  malicious-peer `169.254.169.254` rejection test.
- **terminal** — clean daemon/cli split, thorough vt100 edge-case tests (CJK
  wide-glyph slicing, alt-screen swallowing, incremental diff, teardown).
- **knowledge feature gating** — `protocol` is purely additive; default and
  no-default builds both pass; CI's `--all-features` + `cargo hack
  --feature-powerset` cover the matrix.
- **hostcheck / prompt** — correctly factored as shared crates; no stubs,
  honest `Warn`/`Fail` semantics for optional vs required probes.
- **No leftover stubs** — zero `todo!`/`unimplemented!`/`FIXME` across all
  six crates.

## Cross-cutting

Verified the real dependency graph from every `crates/*/Cargo.toml` plus
grep-driven sweeps across `crates/*/src/`. Bottom line: **the layering is
clean with no cycles or inversions, and the error/secret/panic/blocking
posture is genuinely strong.** The cross-cutting debt is duplication of the
runtime-path/env contract plus a couple of stale markers — not architectural
rot.

### Actual dependency graph (verified) vs intended

```
LEAF (no internal deps):   protocol   netbird   prompt   terminal
knowledge ─→ protocol (opt, feature="protocol")
hostcheck ─→ protocol, netbird
client    ─→ protocol (pub-re-exported), netbird
daemon    ─→ protocol, netbird, terminal, hostcheck, knowledge[protocol]
gui-core  ─→ client, protocol, knowledge[protocol], prompt  (+ keyring, reqwest = providers)
cli       ─→ client, gui-core, protocol, netbird, terminal, hostcheck, prompt
gui       ─→ gui-core, protocol
xtask     ─→ cli, knowledge         (tooling, not shipped)
gui-core  ─→ daemon  [DEV-dependency only, tests/loopback.rs]
```

All deviations from the intended `protocol → client → {cli, gui-core} → gui`
layering are benign:
- **`cli` depends on both `client` and `gui-core`** — intended reuse: the CLI
  `assistant` command delegates to gui-core's assistant core and maps
  `CoreError → CliError` (`cli/src/commands/assistant/mod.rs:369`).
- **`gui-core → daemon` is a dev-dependency only** (used at
  `tests/loopback.rs:14`). No runtime inversion.
- **`daemon` has zero client/gui-core/provider deps.** Providers live only in
  gui-core — the promised trust boundary actually holds.
- **`cli`/`gui`/`gui-core` import `protocol` directly** even though `client`
  re-exports it (`client/src/lib.rs:9`). Defensible, but pick one convention
  (T5). No dependency cycles; `xtask → cli` is build tooling (renders CLI
  help/docs against the live clap tree, `xtask/src/lib.rs:390`).

### Findings (most severe first)

**T1 — P1: runtime-path & socket contract duplicated across 3–4 crates.** The
daemon *binds* the control socket; cli and gui *dial* it, so
`$XDG_RUNTIME_DIR/pohunek/daemon.sock` is a hard IPC contract — yet
independently re-encoded:
- `daemon/src/paths.rs:24-26,66-67` — `APP_DIR`, `SOCKET_NAME`, data/log/
  cache/config dirs;
- `cli/src/paths.rs:12-13,46-47` — near-verbatim copy. **Already drifting**:
  daemon uses a `LOGS_SUBDIR` const, cli hardcodes `.join("logs")`
  (`paths.rs:53`);
- `gui/src/main.rs:4379-4383` — `local_socket_path()` bypasses the consts
  entirely with raw literals; `config_dir()` (`:4385`) re-implements XDG
  resolution inline;
- `gui-core/src/assistant.rs:88,112,520` + `daemon/src/paths.rs:120` — the
  assistant runtime dir computed in both daemon and gui-core.

Why it hurts: this is experimental, no-back-compat code — the layout *will*
change, and three encodings (one already on literals) will silently disagree,
leaving a client unable to find its daemon. *Fix:* a small `pohunek-paths`
crate (or a `paths` module in `protocol`) owning `APP_DIR`, `SOCKET_NAME`,
and the socket/runtime/XDG resolvers; each crate wraps the env error in its
own type.

**T2 — P2: `require_env` fail-fast helper duplicated 4×.**
`daemon/src/paths.rs:135`, `cli/src/paths.rs:90`,
`gui-core/src/assistant.rs:827`, `gui/src/main.rs:4396` — identical "read env
or fail fast" logic differing only in the returned error type. Correct
behavior (no silent defaults), same code four times. Fold into the same
shared home as T1.

**T3 — P2: `gui/src/main.rs` is a 4 819-LOC single-file binary module** with
15 inline tests and zero integration tests — the workspace's thinnest
coverage relative to size. Cross-cutting angle (decomposition is covered in
the GUI section): testable logic — socket path, config parse, update/reducer
logic — is trapped in the binary instead of headless gui-core, which is
exactly what the gui/gui-core split was meant to prevent. *Fix:* push
non-view state transitions down into gui-core; keep `main.rs` view + wiring.

**T4 — P3: RFC3339 timestamp helper duplicated 3× inside the daemon.**
`project/mod.rs:40` (`now_rfc3339`), `worktree/mod.rs:1489` and
`session/mod.rs:1831` (both `timestamp_now`) — identical bodies. Collapse to
one `crate::time` helper.

**T5 — P3: stale "stubs" markers misrepresent maturity.** Root
`Cargo.toml:8-9` ("later modules are stubs") and `daemon/src/lib.rs:30-32`
("future-milestone stubs") — but the daemon is 25k LOC fully implemented and
tested. Delete/update. Related: decide the protocol re-export convention
(direct dep everywhere vs funneled through client).

### Already good, keep

- **Cross-layer error contract is preserved, not stringified.** Every crate
  has a typed `thiserror` enum; typed `ProtocolError` flows end-to-end —
  `ClientError::RemoteProtocol` clones the source preserving
  `{class, code, recover}` and only augments `msg` with host context
  (`client/src/error.rs:113-117`), proven by test (`client/src/error.rs:284`).
  No `anyhow` stringification; the only `(String)` variants are genuinely
  unstructured lower-level failures.
- **Secret posture is sound and tested.** Tokens/keyring/`gh` confined to
  gui-core; the daemon has zero token/auth surface. No `Debug`-derived struct
  owns a token (Linear passes `token: &str` borrowed, never stored). Explicit
  `redact_auth_tokens` covers `ghp_/gho_/ghu_/ghs_/ghr_/github_pat_`
  (`gui-core/src/providers/github.rs:865`), tested against leakage.
- **Blocking-in-async handled.** Blocking git/PTY subprocesses run via
  `spawn_blocking` (`daemon/src/session/target.rs:287,345,389`;
  `pty/mod.rs:318`); the worktree manager documents the contract
  (`worktree/mod.rs:185`). (Timeout-boundedness is a separate issue — see M1.)
- **Panic posture is excellent.** Excluding test code, production
  `unwrap/expect/panic` is ~0 in every crate except daemon, whose ~7 are
  documented invariants (embedded-manifest parse at init, infallible `write!`
  to `String`). No `.lock().unwrap()` poison panics anywhere.
- **Test distribution is healthy except gui**: protocol 106, daemon ~400,
  cli 214, gui-core 83, client 32 tests; providers covered at integration
  level. Only `gui` is thin relative to risk.
