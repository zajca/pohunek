## Unreleased (2026-10-22, stop terminal commit failure, #1402)

- A `session stop` or `session rm` whose worker ended the session but whose
  terminal commit failed (a store outage) no longer leaves the session
  paused: the error is returned, the session stays monitored, the stop or
  removal intent stays pending, and the daemon commits `stopped` as soon as
  the store accepts the write, without a restart. See
  [sessions](concepts/sessions.md).
