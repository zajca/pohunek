## Unreleased (2026-11-26, usage-limit sighting budget split by marker class, #2527)

- The usage-limit screen journal (`session.program_status.limit_screen`) keeps a separate
  budget of 16 sightings per runtime launch for the bare phrases (`usage limit`, `weekly
  limit`) and for the specific phrases. A transcript that quotes a bare phrase can no
  longer spend the budget a real limit hit needs.
- The `further usage-limit screen sightings suppressed` line now carries `marker_class`
  (`generic` or `specific`) and is written once per class.
- A screen that names a bare phrase and later a specific one is sighted once more, as
  the specific sighting; a screen that falls back to a bare phrase is not sighted again.
- The sighting and unmapped-record bounds are per runtime launch, not per session: the
  detector is rebuilt on create, recover and reconcile. `guides/claude-package.md`,
  `architecture.md` and `runbooks/claude-canary.md` say so.
