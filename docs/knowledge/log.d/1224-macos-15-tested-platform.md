## Unreleased (2026-10-16, macOS 14 or newer, tested on macOS 15, #1224)

- Native macOS archives support macOS 14 or newer on Apple Silicon and are
  tested on macOS 15. The deployment target stays 14.0, and the daemon
  installer refuses an older macOS. CI and release acceptance run on
  macOS 15 only; there is no macOS 14 test runner. See
  [the macOS install runbook](runbooks/install-on-macos.md).
