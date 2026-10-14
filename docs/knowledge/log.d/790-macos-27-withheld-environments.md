## Unreleased (2026-10-10, macOS 27 withheld environments, #790)

- On macOS 27 the kernel withholds the environment of restricted Apple platform
  binaries (`/bin/sh`, `/bin/zsh`, `/bin/sleep`). The Darwin inspector now reads
  such a process as unobservable instead of unmarked, so a lost-runtime sweep
  lists every one of them as an unreadable candidate and the cleanup stays
  unconfirmed (`session rm` refuses until they are inspected or
  `--accept-unconfirmed-cleanup` is given). The runbook `debug-session-runtime`
  names the case.
