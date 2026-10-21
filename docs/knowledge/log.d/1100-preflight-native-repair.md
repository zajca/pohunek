## Unreleased (2026-10-21, upgrade preflight repairs native recovery, #1100)

- The upgrade preflight runs reconciliation's native-recovery repair on every
  record and its resume binding before it merges them, over the profiles and
  runtimes the new daemon will have (the planned profile texts included). The
  repair is read-only. A binding that waits for the runtime registry is no
  longer reported at risk when startup would complete it, so the upgrade stops
  asking for `--accept-runtime-loss` where resume would succeed.
- A binding the repair cannot complete is reported `would_lose_recovery` with the
  new code `native_recovery_unrepairable`, whose `detail` names the reason. It
  has no acceptance scope, so accepting it is one-time.
  `native_recovery_unverified` is reported only by a daemon older than this
  change, when it judges a rollback; the CLI does not match on either code.
- No store, journal or report schema change.
