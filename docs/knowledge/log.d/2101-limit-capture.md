## Unreleased (2026-11-25, passive usage-limit capture, #2101)

- A provider usage limit cannot be triggered on demand, so the daemon journals
  the screen instead. A usage-limit screen the detector sees once logs an info
  line under `session.program_status.limit_screen` with the matched marker, the
  held OSC 7501 record in canonical form, its age and staleness, the latest
  unmapped record's bounded tokens and the published activity. The sighting
  changes no activity, attention or input readiness, and no screen or message
  text is logged.
- A record the daemon cannot map is journaled at info, not debug, so the
  evidence reaches the daemon's default `info` filter. See
  [Program status](guides/claude-package.md#program-status-osc-7501); the
  canary runbook (`docs/runbooks/claude-canary.md`, "Usage-limit evidence")
  says how to collect the evidence of a real hit.
