## Unreleased (2026-10-17, one macOS release daemon build per CI run, #1210)

- A new `macos-release-build` job in `ci.yml` builds the `aarch64-apple-darwin`
  release daemon binaries (`pohunek`, `pohunekd`, `pohunek-sessiond`) with
  `packaging/macos/build-release daemon` once per run and uploads them as the
  `macos-release-daemon` artifact. It runs whenever `macos-package` or
  `hermes-compatibility-macos` runs.
- `macos-package` packages version A and `hermes-compatibility-macos` packages
  its daemon archive from the downloaded binaries; neither compiles them. A
  main run builds the release daemon twice (version A once, version B once)
  instead of three times.
- The nightly cold rebuild in `macos-package` now compares its rebuild with the
  builder's binaries, which were built on another runner at the same checkout
  path.
- `scripts/ci-evidence.json` requires the builder in main and nightly evidence.
  `scripts/tests/test_ci_tiers.py` checks that the builder runs exactly when one
  of its consumers runs, and that the consumers download its artifact and
  never build version A themselves.
