## Unreleased (2026-10-14, macOS test lane partitions, #361)

- The macOS test selections are a single nextest archive with two runs:
  `scripts/test-partitions run-macos` runs the nine packages the pull-request
  macOS lane tests, and `run-macos-tier` runs `pohunek-client`,
  `pohunek-netbird`, `pohunek-package` and `xtask`, whose macOS-only code the
  main and nightly tiers cover. `check-macos` proves that every macOS test of
  the 13 packages is in exactly one of the lane, the tier, or a named bucket;
  `check-macos --scope lane` checks the same partition over the nine lane
  packages alone, for the PR lane's archive.
- The `macos` nextest profile sets retries for diagnosis; a flaky result still
  fails the run.
