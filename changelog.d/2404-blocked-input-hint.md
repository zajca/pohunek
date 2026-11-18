---
kind: fixed
---
A `session.new --input` timeout while the agent waits on a dialog now names
the blocked state and points at answering the agent's dialog in the session
(for example `pohunek attach <session-id>`) instead of suggesting a longer
`input.initial_startup_grace_ms`, which only a merely slow start keeps. The
error code stays `session_input_not_ready` and the session is still removed.
([#2404](https://github.com/zajca/pohunek/issues/2404))
