## Unreleased (2026-10-16, macOS archive smoke warms python3 before the consumers, #1192)

- On macOS, `packaging/smoke-archive` starts `python3` as the throwaway user,
  in the sandbox and with the consumer's `PATH`, before each consumer runs,
  prints how long it took and fails the smoke when it cannot start. A fresh macOS user's first start of the
  `/usr/bin/python3` shim took 17 s on macos-15, longer than the agents' 10 s
  hook timeout. Claude killed its first `SessionStart` hook before that hook
  reported the identity, so the session never got a native reference, and the
  v0.35.0 release smoke failed twice. Later starts take about 0.1 s.
- When a release consumer fails, the dump prints every hook line of each agent
  transcript, so a hook the agent killed is visible even outside the log tail.
