## Unreleased (2026-11-18, index-host follow-ups, #2384)

- `package_runtime_conflict` carries `details.holder`: the installed package
  id that serves the runtime id (absent when a built-in runtime serves it),
  so the refusal names the holder on the wire instead of only in `msg`. The
  CLI prints the holder in the human error; JSON clients already see the typed
  `ProtocolError.details`.
- A stored index copy below only the revocation high-water mark is refreshed
  before an install by id, like a copy below the index sequence mark: the CLI
  compares both marks in `read_stored`, and a daemon-side refusal stays the
  backstop (`package_index_rollback`).
