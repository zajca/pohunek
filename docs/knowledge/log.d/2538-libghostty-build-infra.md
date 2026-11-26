## Unreleased (2026-11-26, libghostty-vt build infrastructure, #2538)

- The release and CI builds can provision the pinned libghostty-vt static
  library (`scripts/libghostty-vt.pin.toml`, built by `scripts/build-libghostty-vt`
  with the pinned Zig) through the `libghostty-vt` and `libghostty-vt-macos`
  actions; no Pohunek binary links it yet, so no CLI, protocol or concept
  changes.
- The release evidence attests the pin (library version and commit, both
  tarball sha256, Zig version and sha256, signing key) as
  `libghostty-vt-evidence`, next to the archive checksums.
- Source map: no assistant-visible path changed.
