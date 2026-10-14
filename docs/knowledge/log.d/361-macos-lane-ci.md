## Unreleased (2026-10-14, macOS lane CI jobs, #361)

- The `platform contracts (arm64)` job now lints the nine lane packages with one
  clippy run and runs their tests from one nextest archive: `check-macos --scope
  lane` proves the partition, `run-macos` runs it, the lane packages'
  doctests run natively, and the foreign-owner APFS test runs as root from the
  extracted archive. Its thread count, retries and
  launchd test group come from the `macos` profile.
- The new `macOS tier packages (arm64)` job runs `pohunek-client`,
  `pohunek-netbird`, `pohunek-package` and `xtask` on macOS, with
  `check-macos --scope all` over the 13 macOS packages. It runs on push, the
  daily schedule, and `main` or `nightly` dispatches, never on a pull request,
  and it is a required job of the main and nightly release evidence.
- The `macos` paths filter also gates `scripts/test-partitions`,
  `.config/nextest.toml`, `crates/assistant/**` and `crates/package/**`, which
  the lane's build and test binaries depend on.
