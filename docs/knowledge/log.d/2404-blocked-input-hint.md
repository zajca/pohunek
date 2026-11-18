## Unreleased (2026-11-18, blocked initial-input hint, #2404)

- A `session.new --input` timeout while the session activity is `blocked`
  names the blocked state and tells the operator to answer the agent's dialog
  in the session (for example `pohunek attach <session-id>`) instead of
  raising `input.initial_startup_grace_ms`; a merely slow start keeps the
  grace hint. The code stays `session_input_not_ready` and no protocol field
  changes.
