## Unreleased (2026-10-21, a lost runtime reports how its worker ended, #1330)

- `SessionRuntime` gains an optional `exit` (`RuntimeExit`: an open `cause`
  string, `signal`, `code`). On Linux the daemon reads the failed worker job's
  verdict from the systemd user manager before it retires the job, so a worker
  killed for memory shows `cause=oom_kill`. The field is absent after a host
  restart, on macOS (#1343) and when the read fails. It is an additive optional
  field: no protocol version, store schema or worker-protocol change, hidden
  from protocol 3 peers; see [debugging session
  runtime](runbooks/debug-session-runtime.md).
