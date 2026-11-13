## Unreleased (2026-11-13, fetch_ref tracking refs, #2263)

- The `session.new` `fetch_ref` fetch moves no remote-tracking ref, even one a
  configured `remote.origin.fetch` refspec maps; the daemon's own base and
  branch fetches still move `refs/remotes/origin/*` like an owner's fetch. See
  [Worktrees](concepts/worktrees.md#fetching-one-extra-ref).
