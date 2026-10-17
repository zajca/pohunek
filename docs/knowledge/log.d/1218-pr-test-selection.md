## Unreleased (2026-10-17, pull request CI runs the affected tests, #1218)

- Pull request CI runs only the Linux tests a change can affect. A
  `test selection` job judges the change with the selector of the merge base
  (`cargo xtask affected ci`), and the fast shards, the heavy job, and the
  relay DB job run their shard narrowed to the selected packages. A run
  narrows only when the test pipeline (`.github/`, `.config/nextest.toml`,
  and `scripts/`) equals the merge base's.
  Pushes to `main`, the nightly run, dispatch runs, and any change to the
  pipeline or an unmapped path still run every test. The
  `test selection check` job proves that the planned shards hold every
  selected test, and the `test selection verdict` job fails a run that
  skipped a test job without a narrowed selection.
