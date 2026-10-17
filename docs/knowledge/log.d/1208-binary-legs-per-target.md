## Unreleased (2026-10-17, release build runs one job per target, #1208)

- `release-build.yml` builds the Linux archives in one job per target,
  `Build x86_64-unknown-linux-gnu (cli daemon relay)` and
  `Build x86_64-unknown-linux-musl (cli daemon)`, instead of one job per
  component. Each component keeps its own `cargo build` invocation and is
  staged and archived before the next one builds, so the components share the
  compiled dependencies without changing each other's binaries. The artifact
  names `build-<component>-<target>` are unchanged, and the release path
  (`release.yml`) uses the same jobs.
- The runtime package job also packs the TypeScript SDK tarballs, after it has
  uploaded the package archives and their evidence. The artifact
  `sdk-release-assets` is unchanged.
- A rehearsal builds and signs both macOS components in one job, each with the
  same `packaging/macos/build-release <component>` invocation as the release's
  `stage-macos` job. A rehearsal now runs 12 runner jobs.
