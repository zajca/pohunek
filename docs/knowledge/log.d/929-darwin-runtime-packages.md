## Unreleased (2026-10-14, darwin runtime packages attested, #929)

- `compat/matrix.json` declares `aarch64-apple-darwin` rows for `claude`,
  `codex`, `hermes` and `pi`. The Apple Silicon daemon archive carries
  `runtime/packages` and `runtime/attestations` alongside the signed catalog,
  built and tested natively on macOS 15. The claude lock pins the darwin-arm64
  binary by SHA-256 and enables the aarch64-apple-darwin consumer row.
- Documented in `guides/claude-package`, `guides/codex-package`,
  `guides/hermes-package`, `guides/runtime-packages`, `concepts/runtime-catalog`,
  `concepts/release-bundle`, `runbooks/runtime-package-recovery`, and
  `runbooks/update-after-release`.
