## Unreleased (2026-10-14, far-future subagent claims, #940)

- A subagent record's journaled sequence, and a remembered orphan stop's, is
  capped at the worker's `CLOCK_MONOTONIC` reading when the claim arrives, so
  one far-future start or stop no longer freezes the record until the runtime
  ends. Claims from the host's monotonic clock keep their sequence, and
  comparisons use the claim's own sequence.
- Identity claims are ordered per reporting process since #879, so a
  far-future identity claim only orders its own process's later claims.
