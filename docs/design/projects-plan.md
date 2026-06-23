# Implementation Plan: Projects

Companion to [`projects.md`](projects.md) (the design + resolved decisions). This
turns the five slices into ordered, testable milestones with concrete code
touch-points. Read the design first; this assumes its three decisions.

## Definition of Done

- `session new` in a git work tree, with no flags, runs the agent **in place** and
  silently records a project (`source: auto`). In a non-git dir it is a plain
  shell with `project_id = null`. (Decision 3)
- A project is referenced by `--project <id|label>`; the daemon resolves it
  against its own store. No filesystem path crosses the wire for a remote target.
  (Decision 1)
- `pohunek [--host H] project {list,add,show,rename,rm}` work over a new
  `project.*` protocol family, human + `--json`, with the Phase-5 filter grammar on
  `list`. (Decision 1, 2)
- Ambiguous `<label>` errors with the candidate `id`s and paths; an `id`
  disambiguates. (Decision 2)
- `--branch` (project inferred) still produces a worktree-per-session, now carrying
  `project_id`. (Decision 3 / existing worktree flow)
- `session list` and the rofi switcher **show** the project (label + branch); the
  filter grammar accepts `project=<ref>`. (M6 — design User Value #2)
- The launcher (`pohunek-launch-issue`/`-pr`, `launcher.conf`) uses `--project`
  instead of a required `repo=`. (M7 — closes the original motivation)
- `architecture.md` matches what shipped (projects + single-file store). (M8)
- All new logic is unit/integration tested; `cargo test`, `clippy`, `fmt` clean.

## Correction to the design's storage note

The design says "a new `projects.jsonl` beside the existing two." The actual store
is a **single** owner-private JSON-lines file with internally-tagged records
(`{"kind":"resume"|"worktree", ...}`, `crates/daemon/src/store/mod.rs:101`). The
plan therefore adds a **third record kind** (`Record::Project`) to that one file —
not a new file. `architecture.md:314` (which lists separate files) is likewise
out of date and should be reconciled when this lands.

## New & changed types

### protocol (`crates/protocol`)

- `src/project.rs` (new):
  - `ProjectInfo { id, label, repo_root, git_common_dir, origin_url: Option,
    default_base_branch: Option, source: ProjectSource, is_bare: bool, added_at,
    last_used_at }` — the wire/list shape (mirrors `SessionInfo`'s role).
  - `ProjectSource { Auto, Manual }` (`#[serde(rename_all="snake_case")]`).
  - `ProjectAddParams { path: Option<PathBuf>, name: Option<String>,
    base_branch: Option<String> }`, `ProjectListParams { filters: Vec<…> }`
    (reuse the Phase-5 filter type), `ProjectShowParams`, `ProjectRenameParams`,
    `ProjectRemoveParams { reference, prune_worktrees: bool }`.
  - `ProjectRef` newtype = the `<id|label>` string, plus a `ResolveError` carrying
    the ambiguous candidates for a clean CLI message.
- `src/lib.rs:60` (`mod method`): add `PROJECT_LIST/ADD/SHOW/RENAME/REMOVE`
  constants (`"project.list"` …).
- `src/session.rs:39` `SessionNewParams`: add `project: Option<String>`
  (the id/label ref). `src/session.rs:279` `SessionInfo`: add
  `project_id: Option<String>` and `is_linked_worktree: Option<bool>`. `Option`
  here is **semantic** (no git ⇒ `project_id = None`), not for compatibility —
  this is an experimental project, so wire/on-disk shapes change freely and we do
  not carry compat shims (see "No backward compatibility" below).

### daemon (`crates/daemon`)

- `src/project/detect.rs` (new): the pure detection unit (Slice A).
- `src/project/mod.rs` (new): `ProjectManager` — detection + store glue +
  reference resolution (id/label → record, with collision detection).
- `src/store/mod.rs`: add `ProjectRecord` (persisted shape; superset of
  `ProjectInfo` minus anything derivable), `Record::Project` variant
  (`:102`), and refactor `read_all` → 3-tuple and `write_all(&resume, &worktrees,
  &projects)` (`:235`,`:266`). New store methods: `load_projects`,
  `record_project` (upsert keyed by canonical `git_common_dir`), `remove_project`,
  `find_project` (by id, then label with collision report). Keep the
  rewrite-everything-under-one-lock invariant.
- `src/store/mod.rs:76` `WorktreeBinding`: add `project_id: Option<String>`. No
  on-disk migration: the store may be wiped on upgrade (experimental).

### cli (`crates/cli`)

- `src/main.rs`: add `Commands::Project { action }` mirroring `Host`/`Session`
  (`:80`,`:116`), and a `ProjectAction { List, Add, Show, Rename, Rm }` enum
  mirroring `SetupAction` (`:163`). `--project` flag on `session new` (`NewArgs`,
  `src/commands/session.rs:51`).
- `src/commands/project.rs` (new): `run_list/add/show/rename/rm`, human + `--json`,
  via the daemon client. `--host` routes through the existing
  effective-host plumbing (same as `host`/`session`).
- `src/commands/session.rs`: send the CLI's **own** `std::env::current_dir()` as
  `cwd` for **local** targets; for remote, send no path and require `--project`
  (or `--repo`). Map `--project` into `SessionNewParams.project`.

## Milestones

### M1 — Detection unit (Slice A)

`crates/daemon/src/project/detect.rs`:

```rust
pub struct DetectedProject {
    pub git_common_dir: PathBuf,   // canonical, absolute  (the key)
    pub repo_root: PathBuf,        // main checkout
    pub checkout_path: PathBuf,    // this work tree's root
    pub is_linked_worktree: bool,
    pub is_bare: bool,
    pub branch: Option<String>,    // None on detached HEAD
    pub origin_url: Option<String>,
}
pub fn detect(cwd: &Path) -> io::Result<Option<DetectedProject>>;
```

Exactly the 8-step algorithm from the design (`projects.md` → "Detection
algorithm"), each `git` call timeout-bounded (reuse the setup-script timeout
discipline, `crates/daemon/src/worktree/mod.rs:57`) and **non-fatal**: any failure
→ `Ok(None)`. Canonicalize the key with `canonical_or_original`
(`crates/daemon/src/worktree/mod.rs:178`).

`id` derivation (Decision 2): `"p-" + fnv1a64(git_common_dir.as_bytes())[..8 hex]`.
FNV-1a is dependency-free and deterministic across restarts/platforms, so the id
is stable without persisting a counter. Lives next to `detect` as `project_id()`.

**Tests** (real temp repos via `git init`, no daemon): main checkout; linked
worktree (`git worktree add`); non-git dir → `None`; detached HEAD → `branch:
None`; symlinked cwd canonicalizes to the same key; bare repo → `is_bare: true`.

**Checkpoint:** `cargo test -p pohunek-daemon detect` green; feeding this repo's
own path yields `is_linked_worktree: false`, a temp `git worktree add` yields
`true` with the same `git_common_dir`.

### M2 — Project record & store (Slice B)

Add `ProjectRecord` + `Record::Project`; refactor `read_all`/`write_all` to carry
three lists. `record_project` upserts by canonical `git_common_dir` (the natural
key — re-detecting the same repo updates `last_used_at`, never duplicates).
`find_project(reference)`:
1. exact `id` match → that record;
2. else `label`/`custom_name` match → if exactly one, it; if >1, `Err(Ambiguous {
   candidates })`; if 0, `Err(NotFound)`.

**Tests:** round-trip all three kinds in one file; upsert-by-common-dir dedup;
corrupt-line tolerance preserved (`:248`); `0600` preserved; ambiguous-label
resolution returns all candidates; a worktree record with `project_id` round-trips
and an old record without it loads (serde default).

**Checkpoint:** writing a project then a resume then a worktree leaves all three on
reload; a second project with the same `git_common_dir` updates in place.

### M3 — Session wiring & auto-registration (Slice C)

Daemon `session.new` (`crates/daemon/src/session/mod.rs:569`) target resolution
(Decision 1 order): `params.project` → (local) cwd auto-detect → `params.repo`.

- Resolve `params.project` via `ProjectManager` → checkout path; ambiguous/not
  found → typed protocol error surfaced to the CLI.
- Else if cwd is a work tree → `detect`, **upsert** (`source: auto`,
  bump `last_used_at`).
- Stamp `project_id` + `is_linked_worktree` onto `SessionInfo`.
- Relax the repo+branch co-requirement (`:778`) to: `branch ⇒ (repo given OR a
  project was resolved)`. With a project and `--branch`, the repo for the worktree
  is the project's `repo_root`.
- In-place (no `--branch`) sets `cwd = checkout_path`; no worktree (Decision 3).

CLI (`crates/cli/src/commands/session.rs`): for local targets default `cwd` to the
CLI's own `current_dir()`; for remote send none and require `--project`/`--repo`.
Map `--project`.

**Tests:** daemon-level — `session.new` with a `project` ref binds the right
checkout; with neither project nor repo but a git cwd, auto-registers and stamps
ids; non-git cwd records nothing; `--branch` + project builds a worktree whose
binding carries `project_id`. CLI-level — local send includes cwd; remote without
`--project`/`--repo` is rejected before dialing.

**Checkpoint:** `pohunek session new` in this repo (rebuilt binary) shows a project
in `project list` and the session carries `project_id`.

### M4 — project.* protocol + CLI (Slice D)

Add the five `project.*` handlers to the dispatch table
(`crates/daemon/src/api/handler.rs:145`), each parsing typed params and calling
`ProjectManager`. `project.show` enriches the record with a live
`git worktree list --porcelain` on `git_common_dir`, plus which worktrees pohunek
owns (`WorktreeBinding` with matching `project_id`) and which have live sessions.

CLI `project` subcommand (`crates/cli/src/commands/project.rs`): `list/add/show/
rename/rm`, human + `--json`, `--host` routing, Phase-5 filters on `list`. `add`
with no PATH uses cwd (local only); with PATH treats it as host-local.

**Tests:** handler unit tests per method (params parse, error paths); CLI parse
tests for the new grammar (mirroring `main.rs` setup-sway parse tests); a
script/integration test that `add`→`list`→`show`→`rm` round-trips against a temp
repo; ambiguous-label CLI error lists candidates.

**Checkpoint:** `pohunek project add`, `list`, `show` (shows live worktrees),
`rename`, `rm` all work locally; `--host` is accepted and routes.

### M5 — Worktree linkage & prune (Slice E)

`--branch` with the project inferred creates the worktree off the project's base
branch (`default_base_branch` ?? repo HEAD) and writes `project_id` into the
`WorktreeBinding`. `project rm --prune-worktrees` removes only pohunek-**owned**
worktrees for that project, reusing `WorktreeManager::cleanup_session` ownership
rules (`crates/daemon/src/worktree/mod.rs:308`); it never touches the main checkout
or unowned worktrees, and a plain `project rm` only forgets the record.

**Tests:** worktree binding carries `project_id`; `rm --prune-worktrees` removes
owned, refuses unowned; `rm` without the flag leaves worktrees intact.

**Checkpoint:** launch a worktree session via the Linear launcher path; its binding
shows the project; `project show` lists it; `rm --prune-worktrees` cleans it.

M1–M5 cover the design's slices A–E (detect → store → session wiring → CLI →
worktree). They **produce** project metadata but nothing yet **consumes** it, and
the launcher still hardcodes `repo=`. M6–M8 close those loops.

### M6 — Surface project in `session list` & the rofi switcher

Delivers design User Value #2 ("group sessions by project, show branch/worktree").
Today `session list` human output shows BRANCH + CWD but no PROJECT
(`crates/cli/src/commands/session.rs:487`), and the rofi switcher rows are only
`host/session  agent  state  activity` (`scripts/pohunek-rofi`).

- `session list`: add a PROJECT column (the project `label`, blank for non-git
  sessions) to `render_list_human` and the `--json` shape; add `project=<ref>` to
  the Phase-5 filter grammar so `session list --filter project=ui` works.
- rofi switcher (`scripts/pohunek-rofi`): add the project label (and branch) to the
  row, so the switcher reads `host/session  project  branch  agent  state
  activity`; optionally sort/group by project. The row's first field
  (`host/session`) stays the selection key, so the reconcile logic is untouched.

**Tests:** `render_list_human`/json include project; the filter matches on project;
a `scripts.rs` test asserting the switcher row carries the project column.

**Checkpoint:** `pohunek session list` shows PROJECT; `$layer1+o` switcher rows show
project + branch.

### M7 — Migrate the launcher from `repo=` to `--project`

Closes the original motivation ("pohunek needs repo"). Today
`pohunek-launch-issue`/`-pr` call `session new --repo "$repo"` via
`pohunek_run_session_new` (`scripts/lib.sh:149`) and `launcher.conf` has a
**required** `repo=` (`crates/cli/src/commands/setup.rs` `LAUNCHER_CONF`).

- `lib.sh`: `pohunek_run_session_new` passes `--project "$project"` instead of
  `--repo "$repo"` (still with `--branch` for the issue/PR branch → worktree path,
  Decision 3).
- `LAUNCHER_CONF`: replace required `repo=` with `project=` (a project id/label on
  the target host). Update the `setup` next-steps text and the `scripts.rs` launcher
  tests (`launch_issue_*`, `launch_pr_*`) to assert `--project`.
- Because the launcher targets a host, `project=` is resolved on that host —
  consistent with Decision 1. A first-time host still needs one `project add`.

**Tests:** update the existing launcher integration tests to the `--project` form;
assert no `--repo` leaks and the token-isolation assertions still hold.

**Checkpoint:** with `project=` set, `$layer1+i` issue picker launches a worktree
session with no `repo=` anywhere in the launcher config.

### M8 — Reconcile `architecture.md`

Bring the authoritative architecture doc in line with what shipped:

- **Worktree Isolation** (`architecture.md:264`): a session now also binds a
  *project*; document in-place vs worktree (Decision 3).
- **Configuration, State, and Log Storage** (`architecture.md:301`): the store is
  **one** internally-tagged JSON-lines file with three record kinds
  (resume/worktree/project), not the separate `*.jsonl` files currently listed —
  this fixes a pre-existing inaccuracy, not just the new addition.
- Add a one-line pointer to `design/projects.md` from the relevant section.

**Checkpoint:** `architecture.md` describes projects and the real single-file store;
no contradiction with `design/projects.md`.

## Build order & checkpoints

```
M1 detect (pure)         ── unit tests, no daemon
M2 store record          ── round-trip tests
M3 session wiring        ── first end-to-end: auto-register + in-place
M4 project CLI/protocol  ── full project surface
M5 worktree linkage      ── isolation flow + prune
M6 surface in list/rofi  ── delivers "group by project" user value
M7 launcher → --project   ── closes "pohunek needs repo"
M8 reconcile docs         ── architecture.md matches reality
```

Each milestone ends green (`cargo test && cargo clippy --all-targets && cargo fmt
--check`) and is independently shippable: M1–M2 add no user-visible behavior; M3 is
the first observable change; M4 adds the CLI; M5 closes the worktree loop; M6–M7
deliver the user-facing value; M8 is docs-only.

## No backward compatibility (experimental project)

This is an experimental, single-operator project: **backward compatibility is a
non-goal.** Concretely:

- CLI and daemon are assumed to be the **same build**; we never handle an old CLI
  talking to a new daemon (or vice versa). No `method_not_found` "please upgrade"
  translation, no version-negotiation shims.
- Wire shapes (`SessionNewParams`, `SessionInfo`, the `project.*` params/results)
  change freely; pick the cleanest shape, not the most compatible one.
- The on-disk store may be **wiped on upgrade**. We do not write migrations for
  the new `project` record kind or the `WorktreeBinding.project_id` field; serde
  defaults are used only where they keep the code simple, not as a compat
  guarantee.
- Operator workflow: upgrade all hosts' daemons together (already a
  "session-killing event by design", `architecture.md:290`).

## Risks & mitigations

- **`read_all`/`write_all` refactor** touches every store mutation. Mitigation: it
  is mechanical (2-tuple → 3-tuple) and fully covered by existing + new round-trip
  tests; do it first in M2. It must not corrupt the **live** resume/worktree
  records of running sessions — that is correctness, not back-compat.
- **git latency on session start.** Detection adds a few `git` execs to the hot
  path. Mitigation: timeout-bounded, run once, result cached on the session;
  failure is non-fatal.
- **id stability.** FNV-1a over the canonical common dir is deterministic; the key
  is the path, the id is derived — moving a repo yields a new id (acceptable per
  design; `origin_url` enables future relink).
- **Bootstrapping a remote project** needs its path once (`project add
  <remote-path>` / `--repo`). By design, not a defect.

## Out of scope (unchanged from design)

Filesystem scanning, cross-host unification, GC of stale auto projects, per-project
policy beyond `default_base_branch`.
