## Unreleased (2026-10-13, package archives decoded only after digest authorization, #967)

- `plugin install --catalog` looks the raw archive digest up in the verified
  catalog before it decompresses anything. Bytes the catalog does not list,
  such as a corrupted copy of a listed archive or a file that is not an
  archive, are refused with `package_untrusted` instead of
  `package_archive_invalid`; nothing reaches the zstd decoder. A listed archive
  that does not decode is still `package_archive_invalid`, and the identity,
  version, core and platform checks after decoding are unchanged.
- `package::archive_digest` computes the package digest of raw bytes without
  decoding them.
- `.cargo/audit.toml` ignores RUSTSEC-2026-0344 (ruzstd 0.9.0 decoder memory
  blow-up) until the package format migration to ruzstd >= 0.9.1 (#969).
- The `runtime-package-archive` and `runtime-catalog` concepts, the
  `runtime-packages` and `hermes-package` guides and the
  `runtime-package-recovery` runbook describe it.
