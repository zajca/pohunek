## Unreleased (2026-10-12, built-in parity goldens, #145)

- `compat/codex/builtin-v0.34/` and `compat/claude/builtin-v0.34/` hold golden
  copies of the built-in Codex and Claude descriptors and detection manifests
  of release 0.34.2. The package tests compare the official packages with these
  goldens; the 0.34.2 descriptors declare no transcript tree. While the daemon
  embeds the built-ins, the tests also require the built-in manifests to equal
  the goldens byte for byte and the built-in descriptors to be the goldens
  followed by the package's transcript tree. Documented in the `codex-package`
  and `claude-package` guides and the source map.
