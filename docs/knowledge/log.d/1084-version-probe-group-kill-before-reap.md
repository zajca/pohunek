## Unreleased (2026-10-14, version probe kills its group before reaping, #1084)

- On Linux and macOS, the agent version probe kills the process group it
  started while the exited probe child still holds the group id, and only
  then reaps it. The probe used to reap first and then signal the group, by
  which time the id could name an unrelated process group of the same user.
- When the probe child is no longer waitable, the daemon reports the probe
  as unavailable and sends no signal, because the pid may already belong to
  another process.
