## Unreleased (2026-11-02, catalog anchor ancestor policy, #1917)

- The catalog trust anchor beside the running daemon executable is read with the executable-lookup ancestor policy: on macOS a user-owned directory chain that `admin` can write (stock Homebrew) is accepted, because whoever can rewrite that chain can already replace the daemon. The anchor file policy, root, Linux and the archive verifiers keep the strict rule. See [Environment resolution](guides/environment-resolution.md).
- The `release_runtime_catalog_unavailable` refusal of the `release-catalog` detection step names whether the trust anchor is missing or cannot be trusted (with the fixed fault text).
