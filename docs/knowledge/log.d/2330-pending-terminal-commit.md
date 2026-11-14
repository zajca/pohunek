## Unreleased (2026-11-14, stop through a pending terminal commit, #2330)

- `session stop` and `session rm` right after a session ends no longer fail
  with `worker_operation_failed` "terminal journal commit is still pending;
  retry stop" when the worker's 10 s wait for its terminal commit elapses on a
  loaded host: the daemon sends the stop once more, which waits up to another
  10 s for the commit. A commit still pending after both waits fails as before,
  and the pending stop intent decides the outcome. See
  [Sessions](concepts/sessions.md).
