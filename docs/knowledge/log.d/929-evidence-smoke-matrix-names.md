## Unreleased (2026-10-16, release evidence names the archive smoke matrix legs, #929)

- `scripts/ci-evidence.json` requires both legs of the archive smoke matrix,
  `Archive smoke (network-isolated, ubuntu-latest)` and
  `Archive smoke (network-isolated, macos-15)`, in the main and nightly tiers.
  With the single pre-matrix name, `scripts/release` refused every release
  commit with `missing required job` after pushing it.
- `scripts/tests/test_ci_evidence` checks every required job name against the
  job names Actions can show. It covers the called job of a local reusable
  workflow (the part after ` / `) as well as the caller. A literal matrix
  expands to its exact names: each `${{ matrix.<key> }}` takes the
  combination's value, and a name without an expression gets the values
  appended in parentheses. Only a `fromJSON` matrix leaves an expression open.
  A required name that a matrix rename left behind, or a leg the workflow does
  not run, fails this test.
