## Unreleased (2026-11-15, local-only daemon, #2368)

- `pohunekd --local-only` starts no remote control listener: it asks no
  overlay for an address, binds no TCP socket and logs `remote control
  listener disabled by --local-only; serving local-only`. A repeated flag is
  refused at startup. The isolated daemon of `cargo xtask claude canary
  capture` runs with it and requires the log line before any session, so it
  never tries to bind the host's overlay listener; the installed service does
  not use it ([Debug the daemon](runbooks/debug-daemon.md)).
