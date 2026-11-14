## Unreleased (2026-11-14, conflict generation record, #2145)

- A `conflict` runtime on list, inspect and event responses reports
  `generation_recorded`: `true` when the record names the worker generation
  that stop and remove prove, `false` otherwise. The Rust `can_stop` and
  `can_remove` helpers are false for a conflict that reports `false`, matching
  the daemon's refusal. See [Sessions](concepts/sessions.md) and
  `docs/public-api.md`.
