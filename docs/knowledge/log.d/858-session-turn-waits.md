## Unreleased (2026-10-12, session turn waits, #858)

- New protocol method `session.wait_turn` (Rust SDK `Client::session_wait_turn`)
  waits for the next `turn_completed`, `agent_blocked`, `approval_required`, or
  `exited` event of one or more sessions after an opaque cursor, bounded by an
  agent-chosen timeout (daemon default 5 minutes, ceiling 30 minutes). Results
  carry the next cursor and per-session progress; reasons are `event`,
  `timeout`, `cursor_reset`, and `targets_ended`. Turn events come only from
  admitted provider hook reports for held sessions and from the daemon
  projector, and are recorded after the attention debounce whether or not the
  notification policy keeps an inbox record; a `turn_completed` whose window
  ends while the session works is dropped as a stale stop. `host.inspect` advertises the method with
  `session_wait_turn_supported`. Documented in `concepts/sessions`, the `debug-session-runtime`
  runbook, and `docs/public-api.md`.
