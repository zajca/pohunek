---
type: Concept
id: concept/worktrees
title: Worktrees
description: Pohunek can bind sessions to repository worktrees so agent work is isolated from the main checkout.
source_kind: manual
intents: [project, debug, help]
---

# Worktrees

Pohunek sessions can run in place or in a dedicated Git worktree. A worktree is
requested through `pohunek session new` with a repository or project plus a
branch. The base branch is taken from `--base-branch`, then the project's
configured default, then the repository default. When an explicit base branch
is not present locally, Pohunek fetches that branch from `origin` before it
considers falling back to the repository default.

Worktrees are useful when an agent should make branch-scoped edits without
touching the main checkout. They also make remote project sessions practical:
the daemon resolves the project on the target host and creates the worktree in
that host's filesystem.

Removing a project record never deletes the main repository. The project remove
command has a separate `--prune-worktrees` option for worktrees Pohunek created;
it does not remove unrelated worktrees.

A pohunek-owned worktree is removed with `git worktree remove --force`, which
deletes the checkout but leaves the branch and its commits in the repository.
`pohunek session rm` does that unconditionally, since it is an explicit operator
action. The automatic session retention sweep does not: it keeps any session
whose owned worktree has uncommitted or untracked changes, commits contained in
no other ref, or a state git cannot report (see the Sessions concept's Retention
section). Cleanup is best-effort, so a checkout whose removal failed is reported
rather than silently counted as cleaned, and the leftover directory needs manual
cleanup.

Assistant guidance should preserve this boundary: verify which checkout or
worktree is active before editing, avoid deleting user-managed worktrees, and
prefer explicit project or repository targeting for project work.
