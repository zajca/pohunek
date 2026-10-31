---
kind: fixed
---
`session.wait` and `session.output` with `wait_ms` that are in flight when the daemon is stopped are answered with `daemon_shutting_down` before the daemon exits. They previously ended as a closed connection: `session.wait` listened for a signal only a test harness raised, and `session.output` did not listen at all. A client that treated the closed connection as an unknown outcome now gets the typed code.
