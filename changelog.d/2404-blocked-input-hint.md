---
kind: fixed
---
A `session.new --input` timeout while the agent waits on a dialog now names
the blocked state and says the session was not kept: start the session
without `--input`, answer the agent's dialog in it
(`pohunek attach <session-id>`) and then send the input, instead of suggesting
a longer `input.initial_startup_grace_ms`, which only a merely slow start
keeps. The error code stays `session_input_not_ready` and the session is
still removed.
([#2404](https://github.com/zajca/pohunek/issues/2404))
