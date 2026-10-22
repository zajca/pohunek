## Unreleased (2026-10-22, session new from the remote branch, #954)

`pohunek session new --branch <b> --from-remote` (`SessionNewParams.branch_source:
remote`) fetches `origin/<b>` and starts the worktree from it: a local branch that
is behind is fast-forwarded, one that is ahead is used with a
`branch_ahead_of_remote` warning, and a diverged one is refused with
`branch_diverged` unless `--reset-diverged` (`on_diverged: backup_reset`) keeps it
as `backup/<b>-<short-sha>` and resets it. The result carries `worktree_source`.
`pohunek assistant` takes the same flags, and a host whose daemon does not advertise
`session_new_branch_source_supported` is refused with
`session_new_branch_source_unsupported`. New errors: `branch_fetch_failed`, `remote_branch_not_found`, `branch_diverged`,
`backup_ref_exists`. See [Worktrees](concepts/worktrees.md).
