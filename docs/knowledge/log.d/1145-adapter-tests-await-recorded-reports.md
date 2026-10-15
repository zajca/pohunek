## Unreleased (2026-10-15, adapter report tests await the recorded state, #1145)

- The adapter report scenarios in `crates/daemon/src/session/tests.rs` wait
  for the state the adapters reported instead of reading it once when the
  adapter script finishes. An adapter gives up on its answer after its 0.5 s
  socket timeout, so on a loaded host the script finished before the daemon
  recorded the report (a local full-gate flake of
  `the_public_socket_alone_accepts_adapter_reports_of_a_runtime_with_a_hook_schema`).
- That scenario parks every report until the adapters have given up, then
  waits for the record. It makes the loaded-host order deterministic and
  covers the product rule: the daemon still records a report whose client
  stopped waiting. `session.report_agent` is not cancelled on hangup and does
  not depend on the peer process.
- Product behaviour is unchanged.
