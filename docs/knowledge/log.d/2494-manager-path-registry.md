## Unreleased (2026-11-21, the registry owns the manager PATH, #2494)

- The search path a daemon follows from its service manager now belongs to the
  session registry that reads it, and the registry refreshes it where it
  resolves or launches an agent (`host.inspect`, the doctors, `session.new`,
  `session.resume`), not from a method list in the request dispatcher. Two
  registries in one process no longer share it.
- One `session.new` takes the search path once and uses that value for the agent
  probe, the worktree bind and its hooks, `git`, the checkout-trust check, the
  `SessionStart` hook and the launch; `session.resume` does the same for its
  probe, checkout-trust check and launch. The `git` and
  hook-interpreter lookups and the `PATH` hooks run with no longer read a
  process-wide copy, so `host.inspect` reports `git_available` and a worktree
  hook sees the `PATH` of the registry it belongs to, whichever registry read its
  manager last.
