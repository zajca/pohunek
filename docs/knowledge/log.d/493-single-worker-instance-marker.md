## Unreleased (2026-10-12, single worker-instance marker, #493)

- `POHUNEK_WORKER_INSTANCE_ID` is the only worker-instance marker the process
  inspectors, the runtime sweep, the managed Claude/Codex hooks and the Hermes
  reporter read. A live worker whose journal names a release before 0.33.1 (or
  none) marks its children with another name, so the upgrade preflight reports
  it `would_not_be_adopted` with `worker_marker_legacy`, and startup leaves it
  running as `incompatible` with the same reason; `session stop`, input,
  resize and attach on it keep the code `worker_protocol_incompatible` but name
  `worker_marker_legacy` in `msg` with a `recover` hint. A sweep never
  signals a worker child (`POHUNEK_WORKER_ID`, set only by a worker) that
  carries the swept session's `POHUNEK_SESSION_ID` but no worker-instance
  marker; a lifecycle hook the daemon runs for the session has no worker
  marker and is ignored. Such a child keeps the cleanup unconfirmed
  (`runtime_lost_cleanup_unconfirmed`, removal refused) and names such
  processes by pid in the `session.runtime.sweep_unmarked` WARN. The runtime
  inventory lists a refused worker as `incompatible` with the same reason. Installed hooks of the previous release differ from the
  shipped assets and are reported for reinstall. Documented in
  `update-after-release`, `debug-session-runtime` and `concepts/sessions`.
