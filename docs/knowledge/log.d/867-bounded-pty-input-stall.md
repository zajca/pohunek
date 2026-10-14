## Unreleased (2026-10-13, bounded PTY input stall, #867)

- `service.toml` schema 5 adds the required key `[input]`
  `write_stall_timeout_ms` (3000 on fresh installs). It is a whole millisecond
  count in `1..=600000`. A missing key makes the daemon refuse startup. Schema
  4 files, the layout `main` released before this change, are now an upgrade
  source like schemas 2 and 3: `pohunek service upgrade` migrates them to
  schema 5 keeping every recorded value and their `[waits]` table, and assigns
  the 3000 ms stall bound. Schemas 2 and 3 migrate straight to schema 5 with
  the same default. The value is the longest time one `session.input` or initial input
  write may wait without the agent reading a byte; it restarts after every
  accepted byte. Attached terminal input is not bounded.
- The daemon sends the bound to each worker in `Initialize`. A write that
  exceeds it fails with the new public code `session_input_stalled` (runtime
  class). Part of the plan may already have been written. `session.new` with
  initial input then removes the session it was creating; `session.input`
  leaves the session running.
- A worker that negotiated a protocol below 7 receives no bound and keeps
  unbounded writes.
- The 3000 ms default is below the client's 5 s request deadline, so a
  fire-and-forget caller receives the typed error instead of `request_timeout`.
