## Unreleased (2026-10-13, declared input readiness for Claude, #844)

- A runtime descriptor's `[input]` table may declare `ready`:
  `bracketed_paste` (the default when absent) or `screen_idle`. `screen_idle`
  counts only the screen-sourced idle evidence of the detection manifest, not
  the bracketed-paste enable signal.
- The Claude package declares `ready = "screen_idle"` because Claude drops
  pastes sent between its bracketed-paste enable and the first rendered prompt
  box. Its version stays `1.0.1`; the version bump gate passes against the
  released `1.0.0`.
- A `session.new` with initial input into a `screen_idle` runtime that shows no
  readiness by the end of `input.initial_startup_grace_ms` fails with
  `session_input_not_ready` (runtime class), writes nothing, and rolls the
  session back. It uses the full grace even from clients without
  `extended_input_ready_wait`.
- A descriptor carrying `ready` is rejected by daemons of release 0.34.x, which
  parse `[input]` strictly.
- The runtime packages guide, the Claude package guide, `docs/public-api.md`,
  `docs/architecture.md`, the sessions concept and the setup guide describe it.
