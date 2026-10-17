## Unreleased (2026-10-17, each test runs at most once per OS in a CI run, #1226)

- The `macOS tier packages (arm64)` job leaves out the xtask Hermes harness
  tests (`kind(lib) and test(hermes)`), which the Hermes macOS job runs in every
  run that starts the tier. That is 115 tests, 208 s of macOS test time per
  main run.
- The macOS lane leaves out the `hermes_package` binary whenever the Hermes
  macOS job runs in the same run: always on `main` and nightly, and on a pull
  request when its `hermes_macos` filter matches. The Hermes job runs that
  binary with the ignored real-Hermes tests.
  Under that condition the `test selection verdict` fails a run whose Hermes
  macOS job did not pass.
- `scripts/test-partitions run-macos|run-macos-tier --skip-owned-by
  hermes-macos` implements both, from one `OWNED_ELSEWHERE` selection.
- A new `test duplicates (same OS)` job in the main and nightly tiers runs
  `scripts/test-duplicates` over every JUnit report of the run. It fails on an
  unlisted same-OS duplicate. The four Linux runtime-package duplicates are
  listed in `scripts/test-duplicates.json` until their per-test environment
  audit is done.
