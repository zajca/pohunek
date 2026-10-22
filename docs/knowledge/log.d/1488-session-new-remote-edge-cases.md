## Unreleased (2026-10-22, session new from remote edge cases, #1488)

`session.new` with `branch_source: "remote"` now also refuses with
`worktree_branch_in_use` when another worktree (or the main checkout) is in the
middle of a rebase or bisect that started on the branch; `git worktree list`
shows such a worktree as detached, so the per-worktree rebase and bisect state
is read as well. A `branch` value that git expands, such as `@{-1}`, is an
`invalid_branch`. The in-use check stays in two places on purpose: before the
pre-create hook, so a doomed request has no hook side effects, and under the
repository's worktree lock, which closes the race with a concurrent
`git worktree add`. See [worktrees](concepts/worktrees.md).
