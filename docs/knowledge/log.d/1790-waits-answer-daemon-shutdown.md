## Unreleased (2026-10-31, bounded waits answer the daemon shutdown, #1790)

- `SessionRegistry::wait` selected on `event_log_shutdown`, which only a test harness cancels; the production signal cancels `daemon_shutdown`. The wait arm now watches `daemon_shutdown`, and `session.output` with `wait_ms` races the worker read against it, so both reply `daemon_shutting_down` at the signal instead of ending as EOF at runtime drop. `event_log_shutdown` stays the event-log drain's token.
- Scenarios run the real `pohunekd` with a shell session: a SIGTERM during `session.wait` and during `session.output` with `wait_ms` is answered with `daemon_shutting_down` well inside the wait bound and before the process exits. PR B of #1790 tracks the connection tasks themselves.
