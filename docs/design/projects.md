# Design: Projects (automatic git-repo awareness)

Status: **proposal / for review**. Not yet a committed phase. Where this disagrees
with `docs/architecture.md`, architecture wins until this is merged into it.

## Objective

Make `pohunek` understand *where* a session runs without the user telling it.
Today `session new` is repo-agnostic: you either pass `--repo`/`--branch`
explicitly or you get a plain shell with no repo identity. The vision is the
opposite default: **when a PTY starts in a directory, the daemon feels out git on
that host** — what repository it is, whether the directory is a linked worktree,
which branch is checked out — and records a lightweight **Project** for it. The
user does nothing; projects accrue as a side effect of working.

Two ways a project ever enters the store, and no others:

1. **Automatically**, when a session starts inside a git work tree.
2. **Manually**, via `pohunek project add`.

There is deliberately **no discovery/scan** — pohunek never walks the filesystem
hunting for repos. It only knows what you actually used or explicitly added.

## User value

- `session new` in a repo "just works" — no `--repo` boilerplate for the common
  case (run the agent here, in this checkout).
- The switcher and any future UI can group sessions **by project** and show
  branch/worktree context, because every worktree of a repo shares one stable key.
- `pohunek project list` is an honest, low-noise inventory: only repos you've
  touched, on this host.

## Prior art: how herdr does it

herdr (0.7.x, inspected via `~/.config/herdr/session.json`, version 3) models this
exactly and is worth copying. Each "workspace" carries a `worktree_space`:

| field | example | meaning |
|---|---|---|
| `key` | `/home/u/Code/keboola/ui/.git` | **git common dir** — the identity |
| `label` | `ui` | repo name (basename of `repo_root`) |
| `repo_root` | `/home/u/Code/keboola/ui` | the *main* checkout |
| `checkout_path` | `/home/u/Code/keboola/ui-programmatic-auth` | *this* worktree's root |
| `is_linked_worktree` | `true` | linked worktree vs main checkout |
| `identity_cwd` | `/home/u/Code/keboola/ui-programmatic-auth` | dir the workspace was opened in |

Decisive observations:

- The **main checkout and a linked worktree of the same repo share one `key`**
  (the common dir), so they collapse to one logical project; only
  `checkout_path` / `is_linked_worktree` differ. (Confirmed in herdr's data: two
  workspaces, `ui` and `ui-programmatic-auth`, both keyed on `.../ui/.git`.)
- A **non-git directory** (`kbc-stacks`) has every `worktree_space` field `null` —
  detection fails gracefully, the workspace still exists, just without identity.
- Detection is **derived from the directory**, never configured.

pohunek already shells out to git in the worktree manager, so adopting the same
model costs no new dependency.

## Data model

A new persisted record, keyed by the git common dir (canonicalized, absolute):

```rust
struct Project {
    id: String,                      // stable, derived: "p-" + short hash of git_common_dir
    git_common_dir: PathBuf,         // KEY. `git rev-parse --git-common-dir`, canonicalized
    label: String,                   // custom_name, else basename(repo_root)
    custom_name: Option<String>,     // user override via `project rename`
    repo_root: PathBuf,              // the main checkout
    origin_url: Option<String>,      // `git remote get-url origin`, for cross-host correlation
    default_base_branch: Option<String>, // base for new worktrees; None = repo HEAD at creation
    source: ProjectSource,           // Auto | Manual
    added_at: String,                // RFC3339
    last_used_at: String,            // RFC3339, bumped on each session start
}
```

`SessionInfo` (`crates/protocol/src/session.rs:279`) gains:

```rust
project_id: Option<String>,          // the project this session belongs to (None = no git)
is_linked_worktree: Option<bool>,    // detected at session start
```

`checkout_path` is not a new field — it equals the session's `cwd` /
`worktree_path` for the in-place and worktree cases respectively.

The relationship to the existing `WorktreeBinding`
(`crates/daemon/src/store/mod.rs:76`): a binding gains a `project_id` so
`project show` can list the worktrees pohunek itself created, alongside the ones
git reports.

## Detection algorithm (exact git invocations)

Run in the session's resolved `cwd`, on the host that owns the session, each git
call bounded by a short timeout (reuse the setup-script timeout discipline,
`crates/daemon/src/worktree/mod.rs:57`). **Every step is non-fatal**: on failure
the session still starts, just unregistered.

1. `git -C <cwd> rev-parse --is-inside-work-tree`
   → not `true` (or error): **no project**. Plain shell session, record nothing.
2. `git -C <cwd> rev-parse --path-format=absolute --git-common-dir`
   → `git_common_dir` (the project key).
3. `git -C <cwd> rev-parse --path-format=absolute --git-dir`
   → `git_dir`. `is_linked_worktree = (git_dir != git_common_dir)`.
4. `git -C <cwd> rev-parse --show-toplevel`
   → `checkout_path` (this checkout's root).
5. `repo_root`:
   - not linked → `checkout_path`;
   - linked → `dirname(git_common_dir)` for a normal repo, with the main worktree
     from `git worktree list --porcelain` (first entry) as the authoritative
     fallback (handles relocated/bare layouts).
6. `git -C <cwd> symbolic-ref --short HEAD` → current branch (detached HEAD → None).
7. `git -C <cwd> remote get-url origin` → `origin_url` (optional).
8. `label = custom_name ?? basename(repo_root)`.

Verified empirically against this repo (main checkout: `git_dir == common_dir`;
a temporary linked worktree: `git_dir = .../.git/worktrees/<n>`,
`common_dir = .../.git`, so the `!=` test cleanly flags worktrees).

Canonicalize the key with the existing `canonical_or_original`
(`crates/daemon/src/worktree/mod.rs:178`) so symlinked paths converge to one
project.

## Session flow (how it ties together)

A project is referenced by `--project <id|label>` (Decision 1). A path never
crosses the wire: the daemon resolves `--project` against **its own** store and
turns it into a host-local checkout. Local and remote are the same mechanism;
local additionally gets the cwd shortcut.

`session new` modes (isolation follows Decision 3 — intent-driven):

- **In-place (default, no `--branch`).** Resolve the project (from `--project`,
  else from cwd auto-detection), **upsert it** (`source: Auto`), and run the agent
  in that checkout **as-is** — no worktree. The session records `project_id`,
  `is_linked_worktree`, `cwd == checkout_path`. This is "I opened a terminal here,
  work here."
- **Worktree (`--branch X` [`--base B`]).** Resolve the project the same way, then
  create a worktree-per-session off the base branch
  (`crates/daemon/src/worktree/mod.rs`); the binding references the project. This
  is the isolated-new-work path; the Linear launcher always takes it (it passes
  `--branch`). Explicit `--repo <path>` still works and still wins.
- **No git.** No project resolvable and cwd is not a work tree → plain shell,
  `project_id = None` (today's behavior, unchanged).

Resolution order for the target project:
1. `--project <id|label>` (works for any host; the only option for remote).
2. local cwd auto-detection (local sessions only — you can't "stand in" a remote
   dir).
3. explicit `--repo <path>` (low-level escape hatch; also how a brand-new project
   is first introduced on a host).

Required CLI fix for the cwd shortcut: today an omitted `cwd` falls back to the
**daemon's** `current_dir` (`crates/daemon/src/session/mod.rs:572`). For **local**
sessions the CLI must send **its own** `std::env::current_dir()`. For **remote**,
the CLI sends no local path at all — you must use `--project` (or, first time,
`--repo` with a path valid on the remote host).

## CLI surface

```
pohunek [--host H] project list [--json] [--filter k=v ...]  # known projects on host H
pohunek [--host H] project add [PATH] [--name N] [--base-branch B]  # PATH (on H) or cwd
pohunek [--host H] project show <id|label> [--json]   # details + live `git worktree list`
pohunek [--host H] project rename <id|label> <name>   # set custom_name
pohunek [--host H] project rm <id|label> [--prune-worktrees]  # forget; never deletes repo

pohunek [--host H] session new --project <id|label> [--branch X]  # the everyday path
```

`--host` makes the whole surface symmetric: `project list` against a remote host
shows **that host's** projects and paths, which is how you discover what to pass
to `--project` for a remote `session new`. References are resolved per host;
ambiguous labels error with the candidate `id`s and paths (Decision 2).

Semantics:

- `project add` is just manual detection + upsert; re-adding is idempotent and
  flips `source` to `Manual` (so it's not garbage-collected as stale auto data, if
  GC is ever added).
- `project show` answers "what worktrees does it have" **live** via
  `git worktree list --porcelain` on `git_common_dir`, enriched with which
  worktrees pohunek created (from `WorktreeBinding`) and which have live sessions.
- `project rm` only forgets the record. `--prune-worktrees` additionally removes
  pohunek-*owned* worktrees for that project (reusing
  `WorktreeManager::cleanup_session` ownership rules); it never touches the main
  checkout or unowned worktrees.
- `list` reuses the Docker-style filter grammar from Phase 5 Slice A for parity
  with `session list`.

Explicitly **not** added: `project discover` / any recursive scan.

## Protocol & daemon changes

- `SessionNewParams` (`crates/protocol/src/session.rs:39`): unchanged shape;
  `repo` becomes inferable, so the daemon's repo+branch co-requirement
  (`crates/daemon/src/session/mod.rs:777`) relaxes to "branch ⇒ (repo given OR cwd
  is a project)".
- New request family mirroring `session.*`: `project.list`, `project.add`,
  `project.show`, `project.rename`, `project.remove`. Same newline-delimited JSON
  contract, same human/`--json` rendering split. `SessionNewParams` gains
  `project: Option<String>` (the id/label reference; resolved daemon-side).
- Detection lives in the daemon (it owns the host filesystem and already wraps
  git). For local sessions the CLI forwards its own cwd as the shortcut; for
  remote the CLI forwards only the `--project` reference (no local path).

## Storage

A **third record kind** in the existing unified store, not a new file. The store
is one owner-private JSON-lines file with internally-tagged records
(`{"kind":"resume"|"worktree", ...}`, `crates/daemon/src/store/mod.rs:101`); we add
`{"kind":"project", ...}`, upserted by canonical `git_common_dir`, preserving the
other kinds under the same single-writer lock and atomic temp+rename. The existing
`worktree` record gains a `project_id`. (Both `architecture.md:314` and an earlier
draft of this doc described separate `*.jsonl` files; that is out of date — see the
implementation plan's "Correction to the design's storage note".)

Per-host and **not replicated**, consistent with the architecture's "each host's
daemon is authoritative" stance. The same logical repo cloned on two hosts is two
project records with different `git_common_dir` and (usually) the same
`origin_url`; correlation across hosts is a UI concern for later, not stored.

## Edge cases

- **Detached HEAD / mid-rebase:** branch = None; still a valid project.
- **Bare repo:** no main checkout; `repo_root` falls back to `git_common_dir`'s
  dir and `label` to its basename; flag it so the UI doesn't promise an in-place
  checkout.
- **Repo moved on disk:** `git_common_dir` changes ⇒ a *new* project appears; the
  old one lingers until `project rm`. Acceptable for v1; `origin_url` makes a
  future "merge/relink" feasible.
- **Submodules / nested repos:** detection keys on the *innermost* work tree's
  common dir — the directory you're actually in, which is the intuitive answer.
- **Symlinked paths:** canonicalized into one key.
- **Auto record you don't want:** `project rm` it. If it keeps coming back because
  you keep starting sessions there, that is working as intended; an `ignored` flag
  to suppress re-add is a possible later addition, deliberately omitted now.
- **git slow/unavailable:** detection is timeout-bounded and non-fatal; the
  session starts unregistered and can be `project add`-ed later.

## Slices & definition of done (testable)

- **Slice A — detection unit.** Pure function: given a cwd, return
  `Option<DetectedProject>` (key, label, repo_root, checkout_path,
  is_linked_worktree, branch, origin_url). Tests cover: main checkout, linked
  worktree, non-git dir, detached HEAD, symlinked path. No daemon needed.
- **Slice B — projects store.** `projects.jsonl` load/upsert/remove keyed by
  common dir; corrupt-line tolerance; 0600; atomic rewrite. Round-trip tests.
- **Slice C — auto-registration + reference.** `session new` resolves the target
  by `--project` → local cwd → `--repo` (Decision 1). In-place by default
  (Decision 3); upserts the project and stamps `project_id`/`is_linked_worktree`;
  non-git dir records nothing. Local CLI sends its own cwd; remote sends only
  `--project`. Ambiguous label errors with candidate ids (Decision 2).
- **Slice D — project CLI.** `add`/`list`/`show`/`rename`/`rm` over the new
  protocol methods, human + `--json`, honoring `--host`, with the Phase-5 filter
  grammar on `list`. `show` reflects live `git worktree list`.
- **Slice E — worktree linkage.** `--branch` (project from `--project`/cwd) creates
  a worktree off the base branch; `WorktreeBinding` carries `project_id`;
  `project rm --prune-worktrees` removes only owned worktrees.

## Risks

- **git subprocess latency** on session start: bounded by timeout; detection runs
  once, results cached on the session.
- **Project identity churn** when repos move; mitigated by `origin_url` and manual
  `project rm`.
- **Bootstrapping a remote project** still needs the remote path exactly once
  (`project add <remote-path>` or `--repo`); after that it's label-only. This is
  by design (Decision 1), not a wart — there is no safe way to discover a path on
  another host without being told it.

## Out of scope

- Filesystem scanning / auto-discovery of repos.
- Cross-host project unification (one logical project spanning machines).
- GC of stale auto projects (records are cheap; revisit if noise appears).
- Per-project config/policy (base branch is the only per-project setting for now).

## Decisions (resolved)

1. **Reference projects by `--project <id|label>`, never by cross-host path.**
   Projects are per-host; the daemon resolves the reference against its own store.
   Local sessions additionally get a cwd shortcut ("the project here"); remote
   sessions pick from the host's `project list`. A path enters a remote host
   exactly once, at first introduction (`project add <remote-path>` or `--repo`),
   then it's label-only. This removes the local/remote asymmetry entirely.
2. **`label` is the primary reference; a stable `id` is the collision fallback.**
   `label` defaults to the repo basename. The internal key is always the
   canonical `git_common_dir`. When two projects on one host share a label,
   commands that take `<id|label>` error and list the candidates with their `id`
   (a short hash of the common dir) and path; you then use the `id`. Everyday use
   is human labels; the hash surfaces only on real collision.
3. **Isolation is intent-driven: in-place by default, worktree on `--branch`.**
   No `--branch` ⇒ run the agent in the cwd checkout as-is (matches "open a
   terminal here, work here"). `--branch X` ⇒ worktree-per-session off the base
   branch (the Linear launcher always takes this path). A per-project / global
   `isolation` override is a possible later addition, not built now.
