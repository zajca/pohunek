# Projects — open follow-ups

Tracking list of open items from implementing the Projects feature
([`projects.md`](projects.md) / [`projects-plan.md`](projects-plan.md)). None
block the milestones; each is a decision or a deferred refinement to resolve
later. Recorded so they are not lost. Severity is the impact if left as-is.

Status legend: **open** (needs a decision) · **deferred** (decided to do later) ·
**resolved** (done; resolution noted under the heading).

> **All items below are now resolved** (2026-06-23). Each heading keeps its
> original write-up for context; the **Status** line records the decision taken
> and where it landed in the code.

---

## F1 — `project rm --prune-worktrees` forgets the record even when a worktree is skipped
**Severity:** low–medium · **Status:** resolved — option (b)

Resolution: `remove_project` (`crates/daemon/src/session/mod.rs`) now removes the
record only when nothing was skipped; a skipped worktree keeps the record
(`removed: false`) with the skipped list, so no binding is left dangling. The
live-session filter was also aligned to the non-terminal set (`is_terminal`),
matching `project show`, so a `Starting` session's worktree is protected too.
Covered by `remove_project_prune_skips_a_worktree_with_a_live_session`.

`--prune-worktrees` skips a worktree that a *running* session is using (chosen
behavior: skip + warn) — but the project **record is still removed**. The skipped
worktree's `WorktreeBinding.project_id` then points at a project that no longer
exists, so `project show <that id>` fails while the worktree + session live on.

Options to resolve:
- (a) Keep current behavior (forget the record; skipped worktree's binding dangles
  until its session stops and it is cleaned up some other way).
- (b) If any worktree was skipped, do **not** remove the record either — `rm`
  becomes "remove what you can; refuse the record while live worktrees remain",
  returning `removed: false` with the skipped list.
- (c) Block `rm` entirely (error) when the project has live-session worktrees,
  telling the operator to stop those sessions first.

Recommendation: (b) — least surprising (a partially-pruned project stays listed
so the operator can finish), and keeps bindings consistent with records.

---

## F2 — Store read-modify-write is not atomic across load→write
**Severity:** low (single-operator) · **Status:** resolved

Resolution: `Store::mutate_project` (`crates/daemon/src/store/mod.rs`) does the
read-modify-write entirely under the write lock, with a closure that receives the
freshest record and returns the value to upsert (or `None` to decline). `upsert`,
`touch`, and `rename` (`crates/daemon/src/project/mod.rs`) all route through it,
so a concurrent edit is merged, not clobbered. (`rename` had the same race and was
fixed too.) Covered by the three `mutate_project_*` store tests.

`ProjectManager::register` and `touch` do `load_projects()` then `record_project()`
as two separate locked operations; the write lock is held only inside
`record_project`, never across the read-modify-write. `add` is now a single write
(fixed in review), but a concurrent `rename` + session-start `touch`/`register`
on the *same* project can still lose one update (last writer wins on the whole
record). Benign for a single operator (no duplicate, recoverable by repeating),
but a real TOCTOU.

Fix: add a store-level atomic upsert keyed by `git_common_dir` that loads, applies
a closure, and writes — all under the existing write lock — and route
`register`/`touch`/`rename` through it.

---

## F3 — Too-old git (< 2.31) fails detection silently
**Severity:** low · **Status:** resolved (warn; `host inspect` git version deferred)

Resolution: when `rev-parse --path-format=absolute` fails *inside a confirmed work
tree* (the too-old-git signal, since Step 1 already proved git ran), detection
emits a one-time `tracing::warn!` via `warn_git_too_old`
(`crates/daemon/src/project/detect.rs`) and stays non-fatal (`Ok(None)`). The
optional `host inspect` git-version surfacing is left as a nice-to-have.

Detection uses `git rev-parse --path-format=absolute` (git ≥ 2.31, 2021). On an
older git the call errors, `detect()` returns `Ok(None)` (non-fatal by design),
and **no project ever auto-registers** on that host — silently. Documented in the
`detect` module doc.

Fix: emit a one-time `tracing::warn!` (not `debug!`) when detection consistently
fails on a host so a too-old git is diagnosable, without breaking the non-fatal
contract. Optionally surface git version in `host inspect`.

---

## F4 — Bare-repo in-place session launches in the bare git dir
**Severity:** low · **Status:** resolved — refuse + steer to `--branch`

Resolution: `resolve_target` (`crates/daemon/src/session/mod.rs`) refuses an
in-place session on a bare project with a `bad_request` steering the operator to
`--branch`. For that steer to be truthful, worktree binding had to accept a bare
source: `is_git_repo` (`crates/daemon/src/worktree/mod.rs`) now treats a bare repo
as a valid source (`git worktree add` works on bare repos), so `--branch` on a
bare project creates a usable worktree. Covered by
`in_place_session_on_a_bare_project_is_refused`,
`worktree_session_on_a_bare_project_is_allowed`, and
`bind_creates_a_worktree_on_a_bare_repo`.

An in-place session resolved to a **bare** project launches the agent in the bare
repo's directory (there is no working tree). It keeps the `project_id` association
and is non-fatal (design says "still a valid project, flag it"), but an agent in a
bare dir (full of git internals, no files) is of little use.

Options: leave as-is (flagged via `is_bare`); or refuse an in-place session on a
bare project with a clear error (steer the user to `--branch`, which creates a
worktree). Resolve once we see whether bare projects occur in practice.

---

## F5 — Resume re-detects every resumable session at startup
**Severity:** low · **Status:** resolved — persist on the binding

Resolution: `ResumeBinding` (`crates/daemon/src/store/mod.rs`) now carries
`project_id` / `is_linked_worktree` (serde-default, so an old line still loads).
`persist_resume_binding` captures them from the live session, and `resume_binding`
restores them directly instead of re-running `detect_at` — so a daemon restart
does no per-session git detection, and a detection failure can no longer silently
drop the metadata. `restore_project_metadata` was removed. No migration (the store
may be wiped on upgrade). Covered by
`resume_binding_persists_project_context_for_restart`.

On daemon restart, `resume_binding` re-runs git detection on each resumable
session's cwd to restore `project_id`/`is_linked_worktree` (they are not persisted
on the resume binding). Bounded and best-effort, fine for a handful of sessions;
adds a few `git` execs per resumable session at startup.

Fix if it ever matters: persist `project_id`/`is_linked_worktree` on the
`ResumeBinding` instead of re-detecting (the store may be wiped on upgrade, so no
migration needed).

---

## F6 — `--repo` beats implicit `--cwd` (deviates from design prose)
**Severity:** none (deliberate) · **Status:** resolved — keep code, fix prose

Resolution: the code's order (explicit `--repo` over implicit `--cwd`) is the
intended one — it keeps the worktree source coherent with the project identity and
matches the existing "`--repo` still wins" note. The design prose in
[`projects.md`](projects.md) ("Resolution order for the target project") was
updated to list `--repo` (2) before cwd (3), removing the inconsistency. No code
change.

The design's resolution prose lists `cwd` (2) before `--repo` (3). The
implementation makes an **explicit** `--repo` win over the **implicitly-sent**
`--cwd` for project resolution, so naming a repo is honored and the worktree
source stays coherent with the project identity. (`--project` and `--repo` are
mutually exclusive.) Only observable when both a git cwd and a different `--repo`
are given. Confirm this is the intended order, or flip to match the prose.

---

## F7 — `origin_url` redaction coverage
**Severity:** low · **Status:** resolved — security confirmed

Resolution: reviewed and signed off. `redact_url_credentials`
(`crates/daemon/src/worktree/mod.rs`) strips exactly the RFC 3986 `userinfo`
component — the only place native git carries a secret in a URL — covering the
`https://<token>@host` PAT, `user:password@`, and `ssh://user@` forms, including
multiple URLs in one message. SCP-form `git@host:path` (no secret) and
query/fragment tokens (git never authenticates through them) are intentionally out
of scope. The security boundary is now stated in the function doc and pinned by
`redact_url_credentials_covers_ssh_and_multiple_urls` and
`redact_url_credentials_security_boundary_scp_and_query_are_out_of_scope`.

`origin_url` is captured at detection and run through the worktree module's
`redact_url_credentials`, which strips `scheme://userinfo@host` (incl. the
`https://<token>@host` PAT form). Confirm this covers every secret-in-URL shape we
care about; SSH `git@host:org/repo` carries no secret. No known gap, listed for an
explicit security sign-off.

---

## Out of scope (from the design, restated here for one place)

- Filesystem scanning / auto-discovery of repos — never.
- Cross-host project unification (one logical project across machines) —
  `origin_url` enables a future "relink", not stored as a link today.
- GC of stale auto projects — records are cheap; revisit if noise appears.
- Per-project config/policy beyond `default_base_branch`.
