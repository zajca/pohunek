## Unreleased (2026-10-14, orphaned session TMPDIR cleanup, #1063)

- A session's private `TMPDIR` is `<cache_dir>/session-tmp/<store key>/<session
  id>`. The store key is the first 16 hex digits of the SHA-256 of the metadata
  store's canonical path, so daemons with different stores that share a cache
  directory never share or sweep each other's directories.
- Daemon startup deletes every entry of its own root that no startup evidence
  names. Evidence is a session record, a resume binding, a rejected store line
  that names a session, a worker runtime slot or claimed session (also an
  orphaned worker left running), a service-manager job of the session, or a
  worker journal. A crash, a failed delete
  or a store reset leaves such directories behind, and so does the
  `.pohunek-session-tmp-*` quarantine of an interrupted delete. Each deletion
  is logged at INFO.
- Startup deletes nothing when it cannot tell which directories are orphaned,
  and logs why. A WARN is logged when worker discovery, the service manager's
  job listing (strict, so a job whose inspection raced counts as unknown) or
  the journal scan failed. An INFO is logged when a rejected store line names
  no session.
- The deletion is the same descriptor-bound, identity-checked one a session
  removal uses. An entry it cannot delete, such as a symlink or a regular file,
  is never followed; it is logged as a WARN with its path and kept, and startup
  continues.
