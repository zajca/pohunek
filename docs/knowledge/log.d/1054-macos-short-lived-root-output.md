## Unreleased (2026-10-14, macOS output of a short-lived root, #1054)

- On macOS a session whose agent or shell printed its output and exited
  within about a millisecond of its launch could end with that output empty.
  The worker now starts reading the PTY before it closes its own descriptor of
  the terminal, so the whole output is retained. Linux was not affected.
