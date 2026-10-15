## Unreleased (2026-10-15, shorter xtask Hermes harness tests, #643)

- The controlled fixtures of the xtask Hermes harness tests pause 50 ms
  between typing a prompt and submitting it instead of one second; the fake
  front end needs no settle time. The production `INPUT_SETTLE_WAIT` is
  unchanged.
- The race-window fixtures (a split terminal command line, an unterminated
  panel border, a busy window after Ctrl-C) hold their remaining output for
  0.3 s, fifteen harness polls. The stalled-fixture tests and their
  one-second negative controls keep the 1.5 s stall.
- One healthy refresh carries the sanitized-golden, compatibility-check and
  PTY-descendant-cleanup assertions that three refreshes carried.
- The three TUI diagnosis cases are three tests, so they run in parallel in
  the heavy partition and none is its tail. Under the macOS profile the
  `hermes-pty` test group still runs them one at a time.
- The isolated `rsa-policy` Clippy check is not cached in CI: its target is
  268 MB, and restoring it would add more to the heavy step than the check
  costs in one of its four test threads.
