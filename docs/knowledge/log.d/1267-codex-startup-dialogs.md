## Unreleased (2026-10-19, Codex visible startup dialogs, #1267)

- Codex package 1.2.1 adds precise visible login-selection, API-key-entry and
  incompatible-background-settings blockers, and extends folder trust to the
  captured 20-column wrapping. Full headings and choices/entry hints are
  required; quoted transcript and partial-dialog cases do not match.
- Real daemon API scenarios install the official package and replay captured
  Codex 0.160.0 bytes through a real PTY at 80/160 columns and 20/30 for folder
  trust. Held dialogs outrank stale idle titles, refuse and roll back without
  task bytes; explicit dismissal plus fresh idle evidence delivers once.
- Descriptor schema, readiness enum and `manifest_idle` semantics stay
  unchanged. Positive initialized-screen readiness remains the unscheduled
  proposal [#1298](https://github.com/zajca/pohunek/issues/1298).
- This changes the `pohunek.runtime.codex` package digest; after release, host
  profiles pinned to its old digest are repinned during upgrade. See the
  [Codex package guide](guides/codex-package.md).
