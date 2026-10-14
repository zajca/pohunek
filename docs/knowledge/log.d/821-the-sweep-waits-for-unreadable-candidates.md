## Unreleased (2026-10-12, the sweep waits for unreadable candidates, #821)

- The runtime sweep no longer leaves a cleanup unconfirmed because of a hidden
  process that is only passing: when a pass signals nothing and every remaining
  candidate is an unreadable-marker process, the daemon re-reads the process
  table for up to the sweep grace (`sweep.grace_ms`) before it reports those
  candidates. A process caught between images, or an orphan of a creator that
  has just exited and is about to follow it, no longer blocks a removal or a
  lost-runtime cleanup; a candidate that outlives the grace stays listed and the
  cleanup unconfirmed, exactly as before.
- The wait is shared by every runtime one operation sweeps: the startup
  reconciliation, one background re-check of a session, and one removal or stop
  cleanup each wait at most one sweep grace in total for such candidates, so
  that wait adds at most one grace to startup readiness however many crashed
  runtimes a persistent candidate blocks; the runtimes decided after it stay
  lost with their cleanup unconfirmed.
