## Unreleased (2026-10-20, session rm hook warnings, #747)

`session.remove` returns an optional `warnings` list. A failing or timed-out
worktree `pre-remove` or `post-remove` hook no longer vanishes with the removed
session record: `pohunek session rm` prints it as `warning [hook]: ...` and
`--json` carries it in `warnings`. The removal itself is unchanged and is never
blocked by a hook. The hooks guide in [worktrees](concepts/worktrees.md)
documents the `post-create` / `pre-remove` pair and that both must be
idempotent. See `docs/public-api.md`.
