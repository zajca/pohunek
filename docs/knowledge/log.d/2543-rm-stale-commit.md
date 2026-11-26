## Unreleased (2026-11-26, session rm of a failed session with split runtime identity, #2543)

- `runbooks/debug-session-runtime.md` says that a repeated
  `session_runtime_commit_stale` from `session rm` on a `failed` session with a `lost`
  runtime is not a concurrent commit; the daemon now accepts the rewrite when the
  stored record's two runtime instance copies disagree.
