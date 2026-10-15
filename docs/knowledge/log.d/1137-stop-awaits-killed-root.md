## Unreleased (2026-10-15, a stop awaits a killed root the kernel holds, #1137)

- `pohunek session stop` no longer fails with `timed out waiting for managed
  process exit` when the session's root process, killed after the stop grace,
  is reaped only a little later, as a real agent CLI at a prompt can be on a
  loaded host. After `SIGKILL` the session worker awaits the root's death for
  a post-kill reap deadline of 3 s (the daemon sends it in the worker's stop
  policy) instead of one more 500 ms grace.
- A root still alive after that deadline fails the stop with the root's state,
  for example `in uninterruptible sleep (D)` or a process group it moved to.
