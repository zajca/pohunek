## Unreleased (2026-11-14, worker logs why a runtime faulted, #2354)

- The worker logs a `worker.runtime.faulted` ERROR event with `error.message`
  whenever a session runtime faults, so the cause of a stop or removal that
  fails with `worker_operation_failed` "runtime faulted before its terminal
  journal commit" is in the session's worker log. See
  [Debug a durable session runtime](runbooks/debug-session-runtime.md).
