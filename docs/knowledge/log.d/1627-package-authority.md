## Unreleased (2026-11-05, package authority extraction, #1627)

- The `package.*` operations moved from `crates/daemon/src/session/package_lifecycle/` to `crates/daemon/src/packages/`; the source map and the package recovery runbook name the new paths. No behavior, wire or CLI change.
