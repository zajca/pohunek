## Unreleased (2026-10-14, process environment writes only in a test's own process, #1119)

- `pohunek_test_support::process_env::ProcessEnv::set` and `remove` panic
  unless the calling test runs in a process of its own. A process of its own
  is the child that `process_env::in_own_process()` starts with `--exact`, or
  a `cargo nextest` process per test. Under the single-process `cargo test`,
  a test that changes `PATH`, `HOME` or another variable can therefore no
  longer reach the concurrently running tests that read the environment
  without the lock, such as fixtures whose agents inherit it.
- Every test that changed the process environment starts with
  `if !in_own_process() { return; }`. The daemon agent tests' private runner
  delegates to the shared one.
