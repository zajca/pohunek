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
- A `working` inferred from terminal output opens a turn only while the
  session's runtime holds input evidence (delivered input, never an agent
  report). The turn takes the evidence and gives it back when abandoned, with
  its start; a new runtime generation or the end of the session discards it.
  The repeat window of an unstamped hook report is anchored at the inbox commit
  of the newest turn.
