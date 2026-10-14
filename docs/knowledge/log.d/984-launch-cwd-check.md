## Unreleased (2026-10-14, launch cwd check, #984)

- A session whose working directory no longer exists when the worker launches
  its agent fails with the new public code `session_cwd_missing` (runtime
  class) and starts nothing. The create then runs the existing compensation:
  the worktree binding and the create record are removed once the worker is
  proven ended, so no agent runs in a directory other than its working
  directory (the process would otherwise fall back to `$HOME`).
- The daemon logs worktree binds and removals at INFO, with the removal reason.
  A hook that exits with a failure status, times out, or cannot be spawned or
  waited on is logged at WARN with its event and status; hook output is never
  logged.
