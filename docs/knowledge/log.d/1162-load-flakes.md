## Unreleased (2026-10-15, daemon tests that failed under host load, #1162)

- The Claude native-recovery scenarios (`health_socket` `native_recovery`)
  wait after each hook report until `session.inspect` shows the reported
  conversation as both the active conversation and the recovery target. The
  hook sends the two in separate fire-and-forget requests with a 0.5 s socket
  timeout, so on a loaded host the reference can arrive later, and a fork
  issued in between was refused as `native_identity_unverified` instead of the
  scenario's `native_identity_uncertain`.
- The 24-seed sweep of `native_supersede` runs as four tests of six seeds
  each, with the same seeds. One test took 80-110 s alone and reached the
  180 s slow timeout under load; each batch takes about 20 s and they run in
  parallel.
- The third failure, `conflict_cleanup`'s partial retirement, is #1156.
- Product behaviour is unchanged.
