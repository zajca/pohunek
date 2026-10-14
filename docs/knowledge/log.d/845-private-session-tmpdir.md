## Unreleased (2026-10-14, private TMPDIR per session, #845)

- Every launched session gets `TMPDIR=<cache_dir>/session-tmp/<session id>`,
  a private (0700) directory below the daemon's cache directory, on Linux and
  macOS. It replaces a `TMPDIR` forwarded through the environment allowlist, so
  concurrent agents no longer overwrite each other's temporary files; a profile
  `[env] TMPDIR` keeps its own value and gets no private directory.
- A stop keeps the directory and a resume reuses it; a fork gets its own. Every
  removal deletes it with its content, read-only directories included; a
  failed delete is logged with the path and does not fail the removal.
- New runtime-class errors: `session_tmpdir_unsafe` when the directory or the
  `session-tmp` directory is not a private directory of the daemon's user, and
  `session_tmpdir_failed` when it cannot be created or its path is not valid
  UTF-8. Both fail the launch before a worker starts.
