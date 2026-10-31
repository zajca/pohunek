---
kind: fixed
---
Fire-and-forget `session.input` to a runtime that refuses input while blocked (Hermes, Pi) no longer writes Enter when a dialog drew during the submit delay: the call fails with `session_input_not_submitted` and the typed text stays in the agent's input box. `idle` is documented as ready for input, and readiness for a first input as the runtime package's `[input] ready` gate.
