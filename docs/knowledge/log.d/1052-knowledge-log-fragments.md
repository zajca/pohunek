## Unreleased (2026-10-14, knowledge-log fragments, #1052)

- A change records its knowledge-log entry as its own file,
  `log.d/<issue>-<slug>.md`, with one `## Unreleased (<date>, <topic>, #<issue>)`
  heading and a body, instead of editing `log.md`, so independent changes never
  conflict on the log. `cargo xtask docs check` refuses an unreleased section
  in `log.md` and a fragment that breaks the rules.
- The embedded bundle and `cargo xtask docs build` show the fragments at the
  top of `log.md` as `## Unreleased` sections, newest date first, then by
  issue. `scripts/release` files them under `## X.Y.Z` with
  `cargo xtask docs log fold` in the release commit and removes them;
  `cargo xtask docs log migrate` converts a branch that still edits `log.md`.
- The unreleased sections that shipped in 0.31.0 to 0.34.0 now sit under those
  release headings; the sections added since 0.34.2 are fragments.
