## Unreleased (2026-10-14, Hermes macOS job on nextest, #361)

- The `pinned Hermes compatibility (macOS arm64, model-free)` job runs the
  xtask Hermes harness tests and the real-Hermes `hermes_package` scenario
  through nextest under the `macos` profile instead of `cargo test`, keeps one
  JUnit report per run, and uploads them as `hermes-macos-junit` with a flaky
  summary.
- Under the `macos` profile the tests of xtask's `hermes::tests` run one at a
  time (test group `hermes-pty`): its PTY scenarios take turns on an
  in-process mutex that a process per test cannot share. This also applies in
  the `macOS tier packages (arm64)` job.
- The `hermes_macos` paths filter also gates `.config/nextest.toml`.
