## Unreleased (2026-11-28, protocol wording after the 8..=9 window, #2583)

- Docs, test names and comments no longer describe a connection that negotiated
  worker protocol 7: with the window at 8..=9 that cannot happen. The
  revision-less snapshot shape is now named the journal-snapshot (offline
  journal) case everywhere, and "worker protocol 8" in prose reads "protocol 8
  or newer", since a v9 worker negotiates 9.
