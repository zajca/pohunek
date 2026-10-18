## Unreleased (2026-10-18, bounded git output drain, #556)

Git commands used by worktree binding and session diff drain inherited output
pipes for at most 250 ms after git exits, retaining at most 8 MiB per stream.
Worktree admin commands release the repository lock as soon as git exits and
may use its exit status with partial diagnostics, logging one warning when
capture is incomplete. Commands that parse or return git output fail explicitly
on an expired drain deadline or capture overflow; `session.diff` returns
`session_diff_failed`. No reader threads survive the call and no process group
is signalled after its leader is reaped. See `docs/public-api.md`.
