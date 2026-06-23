# Projects — open follow-ups

Tracking list of open items from implementing the Projects feature
([`projects.md`](projects.md) / [`projects-plan.md`](projects-plan.md)). None
block the milestones; each is a decision or a deferred refinement to resolve
later. Recorded so they are not lost. Severity is the impact if left as-is.

Status legend: **open** (needs a decision) · **deferred** (decided to do later).

---

## F1 — `project rm --prune-worktrees` forgets the record even when a worktree is skipped
**Severity:** low–medium · **Status:** open (behavior chosen, edge to confirm)

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
**Severity:** low (single-operator) · **Status:** deferred

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
**Severity:** low · **Status:** deferred

Detection uses `git rev-parse --path-format=absolute` (git ≥ 2.31, 2021). On an
older git the call errors, `detect()` returns `Ok(None)` (non-fatal by design),
and **no project ever auto-registers** on that host — silently. Documented in the
`detect` module doc.

Fix: emit a one-time `tracing::warn!` (not `debug!`) when detection consistently
fails on a host so a too-old git is diagnosable, without breaking the non-fatal
contract. Optionally surface git version in `host inspect`.

---

## F4 — Bare-repo in-place session launches in the bare git dir
**Severity:** low · **Status:** open

An in-place session resolved to a **bare** project launches the agent in the bare
repo's directory (there is no working tree). It keeps the `project_id` association
and is non-fatal (design says "still a valid project, flag it"), but an agent in a
bare dir (full of git internals, no files) is of little use.

Options: leave as-is (flagged via `is_bare`); or refuse an in-place session on a
bare project with a clear error (steer the user to `--branch`, which creates a
worktree). Resolve once we see whether bare projects occur in practice.

---

## F5 — Resume re-detects every resumable session at startup
**Severity:** low · **Status:** deferred

On daemon restart, `resume_binding` re-runs git detection on each resumable
session's cwd to restore `project_id`/`is_linked_worktree` (they are not persisted
on the resume binding). Bounded and best-effort, fine for a handful of sessions;
adds a few `git` execs per resumable session at startup.

Fix if it ever matters: persist `project_id`/`is_linked_worktree` on the
`ResumeBinding` instead of re-detecting (the store may be wiped on upgrade, so no
migration needed).

---

## F6 — `--repo` beats implicit `--cwd` (deviates from design prose)
**Severity:** none (deliberate) · **Status:** open (confirm)

The design's resolution prose lists `cwd` (2) before `--repo` (3). The
implementation makes an **explicit** `--repo` win over the **implicitly-sent**
`--cwd` for project resolution, so naming a repo is honored and the worktree
source stays coherent with the project identity. (`--project` and `--repo` are
mutually exclusive.) Only observable when both a git cwd and a different `--repo`
are given. Confirm this is the intended order, or flip to match the prose.

---

## F7 — `origin_url` redaction coverage
**Severity:** low · **Status:** open (confirm)

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
