## Unreleased (2026-10-30, daemon runtime drop bound, #1725)

- `pohunekd` now drops its async runtime through a 1000 ms bound after the last shutdown phase. A blocking store write that never returned used to keep the process alive past `daemon_exit_timeout_ms` because tokio waits for blocking threads without a bound; the daemon now logs a warning and exits, abandoning the uncommitted write. The 1000 ms is part of the existing 2000 ms shutdown margin, so no `service.toml` key and no schema changed.
- A blocking store write the session registry tracks is named in the registry shutdown log while it has not returned.
