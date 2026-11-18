## Unreleased (2026-11-18, blocked initial-input hint, #2404)

- A `session.new --input` timeout while the session activity is `blocked`
  names the blocked state and says the session was not kept: start the
  session without `--input`, answer the agent's dialog in it
  (`pohunek attach <session-id>`) and then send the input, instead of raising
  `input.initial_startup_grace_ms`; a merely slow start keeps the grace hint.
  The code stays `session_input_not_ready` and no protocol field changes.
