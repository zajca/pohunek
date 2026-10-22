## Unreleased (2026-10-22, worktree lock wait bound, #1449)

A worktree operation that takes the repository's admin lock waits for it for up to the git command timeout and then runs its git command under its own timeout, so one operation can take up to about twice that bound; a base-branch fetch also runs a few unlocked git steps, each with its own bound. See [worktrees](concepts/worktrees.md) and `docs/public-api.md`.
