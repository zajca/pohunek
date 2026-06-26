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
configured default, then the repository default.

Worktrees are useful when an agent should make branch-scoped edits without
touching the main checkout. They also make remote project sessions practical:
the daemon resolves the project on the target host and creates the worktree in
that host's filesystem.

Removing a project record never deletes the main repository. The project remove
command has a separate `--prune-worktrees` option for worktrees Pohunek created;
it does not remove unrelated worktrees.

Assistant guidance should preserve this boundary: verify which checkout or
worktree is active before editing, avoid deleting user-managed worktrees, and
prefer explicit project or repository targeting for project work.
