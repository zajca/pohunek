## Unreleased (2026-11-21, the registry owns the manager PATH, #2494)

- The search path a daemon follows from its service manager now belongs to the
  session registry that reads it, and the registry refreshes it where it
  resolves or launches an agent (`host.inspect`, the doctors, `session.new`,
  `session.resume`), not from a method list in the request dispatcher. Two
  registries in one process no longer share it. Behaviour is unchanged; only the
  `git` and hook-interpreter lookups still read a process-wide copy until the
  rest of #2494 lands.
