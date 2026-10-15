## Unreleased (2026-10-15, hook admission waits for an unreachable worker, #1088)

- A hook report (active-agent report or release, native-id report, hook
  notification) for a session whose launch pin this daemon does not serve is
  admitted by the hook schema its live worker journaled. When the worker could
  not be asked, because its inspect failed or it was reconnecting, the
  admission fell back to the schema the pin resolves to now, which for an
  unserved built-in pin is none, so a transient read failure refused every
  report. The admission now asks the worker again every
  `WORKER_CONNECT_RETRY` until `worker_connect_deadline` and is decided by
  the schema the worker reports once it is back. Only a worker that answered
  without a known schema, or a session with no worker to wait for (lost,
  conflicted or terminal), falls back to the pin.
- A worker that stays unreachable for the whole bound gets the report refused
  the way a schema refusal is: a notification gets `hook_not_admitted`, and
  the other reports are not recorded. Every refusal of a hook report is
  logged at WARN as `session.hook.report_refused` with the session, the
  operation and the denial or the bound.
