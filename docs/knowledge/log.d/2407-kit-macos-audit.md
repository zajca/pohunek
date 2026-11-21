## Unreleased (2026-11-21, macOS kit build audit, #2407)

- The `kit-macos` job of `release-build.yml` builds the kit binaries with
  `packaging/macos/build-release kit`, the same locked, deployment-target-pinned,
  path-remapped build as the core macOS archives, and runs `audit-macho` on the
  staged kit tree before archiving it. The kit bundle is attested, not signed,
  so `verify-signed` does not apply.
