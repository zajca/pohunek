## Unreleased (2026-10-22, worktree lock wait bound, #1449)

A worktree operation that takes the repository's admin lock waits for it for up to the git command timeout and then runs its git command under its own timeout; finding the lock is a third bounded git step (see #1471), so one operation can take up to about three times that bound. A base-branch fetch also runs a few unlocked git steps, each with its own bound. See [worktrees](concepts/worktrees.md) and `docs/public-api.md`.
