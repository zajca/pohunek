---
kind: fixed
---
A `session.new` refused for its initial input (`session_input_not_ready`) no longer leaves the session listed when its worker stops answering during the rollback: the daemon ends the worker through its supervisor, as `session.stop` does. ([#2453](https://github.com/zajca/pohunek/issues/2453))
