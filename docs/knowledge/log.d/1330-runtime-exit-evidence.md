## Unreleased (2026-10-21, a lost runtime reports how its worker ended, #1330)

- `SessionRuntime` gains an optional `exit` (`RuntimeExit`: an open `cause`
  string, `signal`, `code`). On Linux the daemon reads the failed worker job's
  verdict from the systemd user manager before it retires the job, so a worker
  killed for memory shows `cause=oom_kill`. The field is absent after a host
  restart, on macOS (#1343) and when the read fails. It is an additive optional
  field: no protocol version or worker-protocol change, hidden from protocol 3
  peers. Store schema 11, a no-op migration from 10: a runtime written before
  it records no exit. A release that predates schema 11 refuses the newer
  store; roll back with the `metadata.jsonl.pre-schema-10` backup the migration
  takes (sessions created since are lost) or keep the newer release; see
  [debugging session runtime](runbooks/debug-session-runtime.md) and
  [update after release](runbooks/update-after-release.md).
