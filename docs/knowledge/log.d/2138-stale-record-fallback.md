## Unreleased (2026-11-13, stale program-status record fallback, #2138)

- A `[program_status]` manifest may declare `stale_after_ms` (1..=3600000
  milliseconds). A held OSC 7501 record that disagrees with the screen for
  that long (a `blocked` record over an idle or working screen with no
  dialog, a `working` record over a weak blocker, an `idle`/`done`/`error`
  record over a working spinner) arbitrates as absent until the next record,
  so the screen rules decide. A visible dialog still wins at once; the
  staleness clock runs on no other disagreement. Only the Claude manifest
  declares the key (`stale_after_ms = 30000`); the feature
  `program-status-staleness-v1` owns it and the Claude package 2.2.0 requires
  it, so an older core refuses the package. Each fallback start and end is
  logged at info level under `session.program_status.gate` with the record
  state, the record age, the winning rule and the screen state; the
  `agent_state` event and `session.detection` carry the same frozen values
  as `fallback`, and `session detection` prints them. An idle under the
  fallback resolves a record-backed blocked attention, the way an idle
  record closes it after a dismissed dialog. See
  [Program status (OSC 7501)](guides/claude-package.md#program-status-osc-7501)
  and the architecture section on OSC 7501 program status.
