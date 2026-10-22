## Unreleased (2026-10-22, worktree lock bound includes the lock lookup, #1471)

A worktree add, remove, or prune takes three bounded steps: the `git rev-parse --git-common-dir` that finds the repository's admin lock, the wait for that lock, and the git command itself. Each is bounded by the git command timeout, so one operation can take up to about three times that bound (about 90 seconds), not twice. A base-branch fetch takes the same lock for its fetch command and also runs a few unlocked git steps. See [worktrees](concepts/worktrees.md) and `docs/public-api.md`.
