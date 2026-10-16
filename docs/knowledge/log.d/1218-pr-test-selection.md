## Unreleased (2026-10-17, pull request CI runs the affected tests, #1218)

- Pull request CI runs only the Linux tests a change can affect. A
  `test selection` job judges the change with the selector of the merge base
  (`cargo xtask affected ci`), the fast shards, the heavy job, and the relay
  DB job run their shard narrowed to the selected packages, and they run
  from a sparse checkout, so a test that reads an unmapped path fails.
  Pushes to `main`, the nightly run, dispatch runs, and any change to the CI
  configuration or an unmapped path still run every test, and a
  `test selection verdict` job fails a run that skipped a test job without
  a successful selection.
