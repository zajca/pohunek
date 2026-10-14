## Unreleased (2026-10-14, orphaned session TMPDIR cleanup, #1063)

- Daemon startup deletes every entry below `<cache_dir>/session-tmp` that is
  not the directory of a session with a record or a resume binding: the
  directory of a session whose record went away while a crash, a failed delete,
  or a store reset skipped the cleanup, and the `.pohunek-session-tmp-*`
  quarantine left by an interrupted delete. Each deletion is logged at INFO.
- The deletion is the same descriptor-bound, identity-checked one a session
  removal uses. An entry it cannot delete, such as a symlink or a regular file,
  is never followed; it is logged as a WARN with its path and kept, and startup
  continues.
