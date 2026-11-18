---
kind: added
---
The release build ships the owner-side plugin kit bundle
`pohunek-plugin-kit-<version>-<target>.tar.zst` (`x86_64-unknown-linux-gnu`
and `aarch64-apple-darwin`, holding `pohunek`, `pohunekd`,
`pohunek-sessiond` and `pohunek-plugin-kit`) as the `build-kit-<target>`
artifacts, attested with the other producer artifacts in a release (and by
the `attest-kit` job on the nightly tier) but never part of the
release bundle. The `plugin-kit-publish` workflow publishes them as releases
of `pohunekd/pohunek-plugin-kit` once the owner setup exists (until then it
stays gated): `core-vX.Y.Z` after a release, `nightly-YYYYMMDD-<sha12>`
prereleases with nightlies older than 30 days pruned.
([#1613](https://github.com/zajca/pohunek/issues/1613))
