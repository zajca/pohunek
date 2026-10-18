## Unreleased (2026-10-17, the cli integration tests build as one test binary, #1228)

- The integration tests of `pohunek-cli` are modules of one test binary, `all`
  (`crates/cli/tests/all.rs`), instead of one binary per file. The dependency
  graph links once for the crate: a `src/lib.rs` edit relinks 4 binaries
  instead of 51, and the crate's nextest archive shrinks from about 549 MB to
  97 MB. Editing a test file now recompiles the whole test crate (about 6.5 s
  instead of 0.2-1.3 s). `release_consumer` and `release_upgrade` stay their
  own test binaries.
- The nextest overrides, the `test-partitions` buckets, the CI jobs and
  `dev-bootstrap` select a former binary's tests as `binary(=all) and
  test(/^<file>::/)`; the selected tests are unchanged.
- Tests that re-run their own binary build the `--exact` filter with
  `pohunek_test_support::exact_test_filter(module_path!(), ..)`, and the
  `exact_filter_guard` xtask test refuses a literal `--exact` in integration
  test code.
