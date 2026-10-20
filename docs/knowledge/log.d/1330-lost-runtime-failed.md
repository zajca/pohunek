## Unreleased (2026-10-20, a lost runtime with an observed end is a failed session, #1330)

- A session whose runtime was lost with an end the daemon observed (the worker's
  job ended and was retired, a host restart, or an answering worker whose child process is gone)
  is `failed` with no activity and still reports `runtime.state=lost` with its
  `loss_reason`. `session wait-turn` ends with `exited`, `session stop` returns
  `stopped: false`, and `session resume` still recovers it. A loss without that
  proof keeps the session state and clears only the activity. Operations that
  need a running session still answer `session_runtime_lost`. No protocol,
  store or worker-protocol version changes; see
  [debugging session runtime](runbooks/debug-session-runtime.md).
