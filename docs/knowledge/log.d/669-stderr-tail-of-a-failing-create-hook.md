## Unreleased (2026-10-14, stderr tail of a failing create hook, #669)

- A failing `pre-create` or `post-create` hook's `hook` warning in the
  `session.new` result ends with `; stderr tail:` and the sanitized last bytes
  of the hook's stderr, marked when cut. The stored session, `session.list` and
  the logs keep the warning without the tail; stdout is never kept.
- The hook's stderr is a pipe the daemon drains. A background process the hook
  leaves holding it gets `SIGPIPE` after a daemon restart (macOS, unsupervised),
  so the daemon logs a WARN naming the hook when the pipe is still open two
  seconds after the hook ended; the runbook explains the fix. #1046 tracks a
  detached sink.
- `service.toml` schema 6 adds the required `[hooks] stderr_tail_bytes`
  (`0..=32768`, `0` disables). Fresh installs and upgrades from schemas 2 to 5
  write 4096.
- The RFC security section (B.3.2), `docs/public-api.md`, the setup guide, the
  worktrees concept and the update runbook describe the capture and its
  exposure.
