## Unreleased (2026-10-28, daemon shutdown tree, #1625)

- Daemon shutdown runs through one tree of ordered phases (creates, drivers, registry, event logs) instead of nine separately bounded steps whose worst case (47 s) exceeded the 30 s service stop timeout. The event-log flush is now the last phase and always gets its own bound. A phase that outlives its bound is named in the log; drivers are stopped cooperatively and never aborted mid-write.
- New optional `[deadlines]` keys in `service.toml`: `create_drain_ms` (10000), `driver_shutdown_ms` (5000) and `event_log_flush_ms` (5000), beside `registry_shutdown_ms` (5000). Their sum plus a 2000 ms margin must fit inside `daemon_exit_timeout_ms`; `pohunekd` refuses to start on a file that does not, naming the keys. The defaults fit the default 30000 ms.
- `SessionRegistry::shutdown` now drains in-flight creates itself.
