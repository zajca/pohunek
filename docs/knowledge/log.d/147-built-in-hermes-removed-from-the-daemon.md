## Unreleased (2026-10-13, built-in Hermes removed from the daemon, #147)

- The daemon compiles no Hermes in: the official `pohunek.runtime.hermes`
  package, installed from the release catalog, is the only source of
  `hermes`, as the Codex and Claude Code packages are of `codex` and `claude`.
  The shell is the only built-in runtime. On a host without the package, a
  bare `hermes` is `agent_profile_not_found` and an unpinned `base = "hermes"`
  host profile answers `runtime_not_installed`; each recovery hint names
  `pohunek plugin install runtime/packages/hermes.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). While the package serves `hermes`, an unpinned profile
  over it needs that migration.
- The release catalog lists the Hermes package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Hermes (#929).
- A session launched from the built-in Hermes of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `hermes`; its hint
  names the install and `pohunek session new --agent hermes`. With the package
  installed the live session can be used again, resume answers
  `runtime_served_by_package` and fork `agent_fork_unsupported` (Hermes
  declares no native fork). `pohunekd upgrade-preflight` reports every live
  built-in Hermes session as `would_lose_recovery`
  (`runtime_source_not_served`) whether or not it could resume; with the
  package installed before the upgrade a non-resumable one is `adoptable`.
  No earlier release archive carries the package to install before the
  upgrade.
- Disabling or uninstalling the Hermes package leaves `hermes` unserved; no
  official alias has a built-in to fall back to.
- The compiled `hermes-v1` version-probe parser is removed. The compiled
  parsers are `semver-v1` and `semver-line-v1`; a descriptor naming any other
  parser id parses, but its runtime reports `supported: false` and a launch is
  refused with `agent_runtime_unsupported`. The probe sandbox sets no Hermes
  home or Python variables and passes the launch `PATH`.
- `daemon.doctor` checks the embedded manifest only for `shell`
  (`agent_manifest_embedded_shell`); `agent_manifest_embedded_hermes` is gone.
- `package.bind_profile` for a `hermes` base no package serves answers
  `package_profile_target_invalid`; `package_profile_base_builtin` is the
  shell's only.
- A `hermes` process outside pohunek or in the foreground of a shell session
  is recognized only through the installed package's detection manifest.
- The `no_capable_agent` hint names the official `codex`, `claude` or
  `hermes` package install from a release archive whose catalog lists the
  platform, or `--agent` with a capable host profile; a `hermes` on the
  daemon's `PATH` alone does not make the assistant capable.
- The v0.34.2 built-in Hermes descriptor and manifest are frozen as goldens in
  `compat/hermes/builtin-v0.34/`; the package tests compare the package with
  them, and a new clean-host scenario covers the refused bare launch, the
  assistant without a capable agent and the launch after the catalog install.
- `pohunek integration ... --agent hermes` stays the CLI-run Hermes operator
  plugin lifecycle; `hermes` stays a reserved runtime id. The hint of a bare
  `integration install` that answers `integration_no_managed_runtime` names
  only the official packages whose hooks the daemon installs (Codex, Claude
  Code), never `hermes`.
- Documented in `guides/hermes-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`,
  `concepts/runtime-package-archive`, the `runtime-package-recovery` and
  `update-after-release` runbooks, the index, the source map, `README.md`,
  `runtime-packages/README.md`, `compat/hermes/README.md`, `docs/install.md`,
  `docs/architecture.md`, `docs/cli.md`, `docs/public-api.md` and
  `docs/acceptance/README.md`.
