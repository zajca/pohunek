## Unreleased (2026-11-18, daemon flake batch, #2401)

- #2401: the idle-backoff procwatch tests no longer measure scheduler delay.
  The paused clock sections run under `AutoAdvanceInhibitor`, so the clock
  moves only through explicit `advance`, and each `advance_settling` step
  overshoots the tick by `TIMER_TICK`, so a scan timer armed for the tick
  boundary fires without relying on auto-advance. Too-frequent scans fail
  the count bound, executor-thread scans fail the executor-thread counter,
  and scans slower than the backoff stall the paused clock and fail the
  due-scan hang guard.
- #2018: not reproduced (15/15 local stress iterations green); the
  best-effort `rollback_failed_initial_input` design stands, and the
  leftover-session instrumentation from #2331 already on this head waits for
  the next occurrence. No code change.
- #2021: not reproduced (30/30 local stress iterations green); the shutdown
  `ShutdownReport` instrumentation from #2331 already on this head waits for
  the next occurrence, and the 5 s shutdown bound is unchanged. No code
  change.
