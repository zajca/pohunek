## Unreleased (2026-10-13, verify-inventory checks package digests before decoding, #972)

- `cargo xtask release verify-inventory` takes a required `--anchor <file>`
  and refuses with a trust-anchor mismatch when any daemon archive ships
  another anchor, before it verifies the catalog or reads a package. Each
  top-level package archive is then compared with the digest of the catalog
  verified under that anchor before it is decompressed, so a tampered package
  is refused as a package-archive mismatch without reaching the decoder
  (`concepts/release-bundle.md`).
