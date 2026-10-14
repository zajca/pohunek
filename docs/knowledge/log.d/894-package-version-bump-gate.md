## Unreleased (2026-10-13, package version bump gate, #894)

- `cargo xtask package bump` fails a runtime package whose archive differs
  from every released archive of its runtime while its `runtime.toml` version
  is not strictly above the released versions (semver). It reuses the verdicts
  of `package baseline` and adds the decisions `pass`, `undecided` (catalog
  absent) and `fail` (`version_not_bumped`, or a catalog that does not verify).
- The `package version bump` pull request job in `ci.yml` compares with the
  last published release at or below the workspace version.
- The Codex package is bumped to `1.1.0` for its transcript tree declaration.
- The rule is in the runtime packages guide; the archive concept describes
  `version_not_bumped` and the bump decisions.
