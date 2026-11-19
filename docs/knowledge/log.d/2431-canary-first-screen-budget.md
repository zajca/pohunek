## Unreleased (2026-11-19, canary capture first-screen budget, #2431)

- `cargo xtask claude canary capture` takes `--first-screen-timeout-secs`,
  the bound of the wait for the first screen of a session (the step timeout
  when absent, refused when zero like it). It bounds only that setup wait:
  a loaded host no longer stops the run at an earlier screen than the step
  the scenario tests; the record and the first notification waits keep
  `--step-timeout-secs`, and a first-screen timeout names `wait for the
  first screen` with its own budget. No daemon, protocol or CLI change.
