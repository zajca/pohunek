## Unreleased (2026-10-20, daemon-emitted turn_completed, #955)

- The projector reports a `turn_completed` when a session's activity goes
  `working` to `idle`, held for `attention_debounce_secs`, so runtimes without
  managed hooks report finished turns and complete `session.wait_turn` on
  `turn_completed`. A hook report and the projector's report of one turn are one
  event; a turn that ends `blocked` reports nothing.
- `NotificationRecord` gains the optional, daemon-set fields `turn_started_at`,
  `turn_duration_ms` and `session_metadata`. They are additive: an older client
  ignores them, and a producer cannot send them.
- The kind stays disabled for the inbox by default; see
  [sessions](concepts/sessions.md).
- Each turn has a daemon-side identity and owner; the projector reports a turn
  from an internal edge, and a turn whose agent or runtime leaves while it
  works is never reported. `session.wait_turn` is at-least-once for turns the daemon did not
  observe.
- A `working` inferred from terminal output opens a turn only when input
  reached the session's current runtime or an agent reported activity; the
  evidence is used up by the turn it starts and reset by a resume, replacement
  or re-adoption, so start-up output is never a turn.
