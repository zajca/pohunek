## Unreleased (2026-10-12, built-in Claude removed, #146)

- The daemon compiles no Claude in: the official `pohunek.runtime.claude`
  package, installed from the release catalog, is the only source of
  `claude`, as the Codex package is of `codex`. On a host without it, a bare
  `claude` is `agent_profile_not_found`, an unpinned `base = "claude"` host
  profile and `integration.install` for `claude` answer
  `runtime_not_installed`, and each recovery hint names `pohunek plugin
  install runtime/packages/claude.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The built-in runtimes are `shell` and `hermes`.
- A session launched from the built-in Claude of an earlier release (pinned,
  or a v0.33.0 record without a launch binding) answers
  `runtime_builtin_removed` on resume, fork and every interaction (input,
  attach, resize, rename, metadata) while no package serves `claude`; with the
  package installed the live session can be used again and resume and fork
  answer `runtime_served_by_package`. `pohunekd upgrade-preflight` reports
  such a session as `would_lose_recovery` (`runtime_source_not_served`)
  whether or not it could resume.
- The release catalog lists the Claude Code package for the Linux x86_64
  targets only, so a macOS daemon has no source of Claude Code (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `claude` work without the package: the core-owned Claude handler keeps
  the config home of the 0.34 built-in (`CLAUDE_CONFIG_DIR`, else
  `~/.claude`), in the runtime's home and, with `profile` or `all_profiles`,
  in the homes of host profiles over `claude`. Install never resolves through
  it.
- New `configuration/integration_home_ambiguous_owner`: an integration request
  without `agent` that reaches one config home through one handler for
  several runtimes (a package declaring `claude-hook-v1` and Claude's
  `[config_home]` under another runtime id) attributes the home to none of
  them. Each runtime reports the error for it: a bare install fails,
  `all_profiles` lists it in `failed`, and status and doctor report the home
  unresolved. A request naming `agent` addresses only that runtime.
- A foreground `claude` in a shell session and an unmanaged `claude` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Claude
  Code.
- `daemon.doctor` checks embedded manifests only for `shell` and `hermes`.
  `pohunek integration --agent` offers only `hermes` as a built-in name;
  `claude` and `codex` are accepted as runtime ids.
- New `runtime/integration_no_managed_runtime`: a bare `integration install`
  on a host where no served runtime has daemon-managed hooks; its hint names
  the official package install and then `integration install --agent
  <runtime>`. A bare install resolves every selected home before it writes,
  so an ambiguous home refuses the request with no home written. The status
  warning of an unresolved home carries the error's hint; the integration
  doctor reports an ambiguous home as `config_root_invalid` with a remediation
  naming `--agent`, and `daemon.doctor` as a warning.
- `--all-profiles` covers host profiles over a served runtime that do not
  resolve (unpinned over a package-served alias, or pinned to a version that
  is not installed): status, doctor and uninstall reach their own `[env]`
  home, and install lists them in `failed`.
- A shell session hosting an agent whose runtime is not served (a Claude
  identity reported by leftover hooks without the package) stays mutable:
  only the session's own runtime must resolve; a nested agent is checked
  against the runtime-id grammar.
- The `no_capable_agent` hint names the official `codex` or `claude` package
  install from a release archive, a supported `hermes` on the daemon's PATH, or
  `--agent` with a capable host profile.
- Documented in `guides/claude-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `README.md`, `docs/install.md`, `docs/architecture.md`, `docs/cli.md`
  and `docs/public-api.md`.
