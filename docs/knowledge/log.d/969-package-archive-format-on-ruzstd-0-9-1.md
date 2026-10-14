## Unreleased (2026-10-13, package archive format on ruzstd 0.9.1, #969)

- Package archives are encoded and read with `ruzstd` 0.9.1, which fixes
  RUSTSEC-2026-0344; the `.cargo/audit.toml` ignore is gone. The encoder writes
  other bytes than 0.9.0 for package-sized inputs, so this is a format
  cut-over: 0.35 refuses archives built before it (including the 0.34.x
  release archives) and 0.34.x refuses archives built by 0.35. Installed
  packages keep working across the upgrade, because the registry never
  re-reads archive bytes. `NonCanonicalCompression` names the rebuild.
- An encoder-drift guard pins the digests of a large generated sample and of
  the released v0.34.2 Claude package's content under the current encoder, and
  a test pins that the v0.34.2 archive is refused.
- The release upgrade test takes the previous release's archive digest from
  its raw bytes and its descriptor from a plain decode, and after the upgrade
  the head links a probe package directory, so its daemon installs an archive
  of the head's format next to the pinned package.
- `concepts/runtime-package-archive.md` describes the format generations.
