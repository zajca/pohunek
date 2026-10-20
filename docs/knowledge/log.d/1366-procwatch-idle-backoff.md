## Unreleased (2026-10-20, idle session load, #1365 #1366)

The daemon publishes `agent_state` only when a session's activity or its source
changes, so an idle session no longer produces an event and a revision every
800 ms; a session with a pending `session.input` wait still gets
its unchanged activity restated as newer evidence. Input and session waiters
ignore events that name another session before they lock the registry, and the
detector skips the manifest evaluation on a tick over unchanged terminal
context. Procwatch scans each session's process tree on a blocking thread and
waits up to `procwatch_idle_poll_max` (5 s) between scans of a quiet session
whose tree has not changed.
