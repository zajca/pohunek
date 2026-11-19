## Unreleased (2026-11-18, kit bundle and gated publishing, #1613)

- The release build ships the owner-side plugin kit bundle
  `pohunek-plugin-kit-<version>-<target>.tar.zst` for
  `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` (the `kit-linux` and
  `kit-macos` jobs of `release-build.yml`, on tag releases and the nightly
  tier only — the schedule or a `tier=nightly` dispatch — never on pull
  request or push runs). The bundle
  holds `pohunek`, `pohunekd`, `pohunek-sessiond` and `pohunek-plugin-kit`
  plus the descriptor schema note; it carries no release MANIFEST and never
  enters the release bundle. The `attest` job of `release-evidence.yml`
  attests it with the other producer artifacts in a release, and the
  `attest-kit` job attests it on the nightly tier, so a downloaded bundle
  verifies with the same signer workflow identity as the existing release
  assets.
- The new `plugin-kit-publish` workflow publishes the bundles as releases of
  `pohunekd/pohunek-plugin-kit`: a core release tag `vX.Y.Z` becomes
  `core-vX.Y.Z`, a scheduled nightly CI run becomes the prerelease
  `nightly-YYYYMMDD-<sha12>`; the release is created as a draft with
  `SHA256SUMS`, uploaded, then published, and nightlies older than 30 days
  are pruned. The job checks out nothing, runs no release-asset code, and
  stays gated on `vars.PLUGIN_KIT_APP_ID` until the owner has created the kit
  repository, its GitHub App and the `plugin-kit-publish` environment.
