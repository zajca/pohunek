---
kind: added
---
`cargo xtask plugin build` writes the canonical, byte-reproducible archive of a plugin's package directory, `cargo xtask plugin prove` installs an archive through the plugin index path on an isolated daemon without launching the runtime and records the verdict as evidence, and the reusable `plugin-release.yml` workflow builds, attests and publishes a plugin release from a version tag. ([#1936](https://github.com/zajca/pohunek/issues/1936))
