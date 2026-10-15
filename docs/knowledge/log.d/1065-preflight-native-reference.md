## Unreleased (2026-10-15, upgrade preflight native reference, #1065)

- `pohunekd upgrade-preflight` runs the native reference check resume runs
  before it relaunches a stopped session, and reports a refusal with resume's
  own code and message.
- A stopped session whose conversation is absent from its declared store
  (`agent_native_reference_missing`), or whose stored reference is malformed, is
  `already_not_resumable` when the upgrade does not change the check. `service
  upgrade` then does not require `--accept-runtime-loss` for it, and `service
  check` still lists it.
- A relocated store (`agent_native_store_relocated`) or a store-condition
  refusal is `would_lose_recovery` with resume's code: the running release may
  detect those differently, so the preflight cannot prove it refuses too.
- A session the upgrade adopts from the built-in runtime stays
  `would_lose_recovery` when its reference check fails.
