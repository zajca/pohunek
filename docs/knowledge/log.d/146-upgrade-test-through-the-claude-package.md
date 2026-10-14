## Unreleased (2026-10-12, upgrade test through the Claude package, #146)

- The release upgrade test installs the official Claude package from the
  previous release's archive under its signed catalog, pins the fake-agent
  profiles with `plugin profile migrate`, and starts its session from the
  package; it asserts the preflight judges that session `adoptable` and that
  the store keeps the same package pin across adoption and `resume`.
  `scripts/upgrade-test` refuses a previous release without
  `runtime/packages/claude.tar.zst` (older than v0.34.2) or with an expired
  catalog. The `update-after-release` runbook describes moving Claude and Codex
  work onto the official package before upgrading to a release that does not
  serve the built-in runtime.
