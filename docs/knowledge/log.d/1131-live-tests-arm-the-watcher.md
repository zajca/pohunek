## Unreleased (2026-10-15, live transcript tests arm the watcher, #1131)

- The live transcript scenarios in `crates/daemon/src/external/live_tests.rs`
  arm the watcher before they change the tree. A probe transcript is
  rewritten until a live event indexes it. A backend that starts delivering
  events only after it reports the root registered (seen as a macOS CI flake
  of `appending_the_identifying_record_indexes_a_transcript`) no longer makes
  a scenario depend on the periodic pass.
- A wait for the first changes after start is bounded by 10 s, a third of
  `RECONCILE_INTERVAL`, so the pass scheduled at start can never satisfy it.
  State waits, and waits for changes made after the scenario caused a pass,
  keep 30 s: such a pass restarts the stream (#1151).
- Product behaviour is unchanged: a change the backend missed is indexed by
  the next pass, at most `RECONCILE_INTERVAL` later. #1151 tracks the
  `FSEvents` stream restart on every pass.
