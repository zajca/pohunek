DRAFT for #1659: namespace ruling implemented (data-driven, per manager)

Manager ruling (2026-11-12): refuse `ExplicitDigest`/`link` of a `pohunek.runtime.*` id ONLY when the verified stored official index lists that package id (data-driven, no hardcoded exemption list); before admission the documented Pi `--sha256` flow keeps working.

What shipped in this lane (not posted, for manager review):
- Daemon guard `refuse_official_namespace` in `crates/daemon/src/packages/mod.rs` (`prepare_local` / `prepare_directory`); fires only when `index_state.reserved_runtime_ids` contains the runtime whose `pohunek.runtime.<id>` is claimed. `LocalTrust` floor for shell/codex/claude/hermes unchanged.
- New error kind `PackageErrorKind::NamespaceReserved` (`package_namespace_reserved`, recover: install by id or from the release catalog).
- Scenario `namespace_guard_data_driven_both_sides` in `crates/daemon/src/session/package_scenarios/index_tests.rs`: `pohunek.runtime.aurora` by `--sha256` works before stored index lists it; refused `NamespaceReserved` after.
- Docs: `docs/public-api.md` (code table), `docs/knowledge/log.d/1614-stored-plugin-index.md`.
- Pi flow preserved: `docs/knowledge/guides/pi-package.md` §Install (`pohunek.runtime.pi` by `--sha256`) and `crates/cli/tests/pi_package.rs` unchanged; shipped/prefix flows (Hermes release via `PackageTrust::Catalog`, fixtures) unaffected.
