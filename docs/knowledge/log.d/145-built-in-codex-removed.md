## Unreleased (2026-10-12, built-in Codex removed, #145)

- The daemon compiles no Codex in: the official `pohunek.runtime.codex`
  package, installed from the release catalog, is the only source of `codex`.
  On a host without it, a bare `codex` stays `agent_profile_not_found`, an
  unpinned `base = "codex"` host profile and `integration.install` for
  `codex` answer `runtime_not_installed`, and each recovery hint names
  `pohunek plugin install runtime/packages/codex.tar.zst --catalog
  runtime/runtime-catalog.json` (the profile hint then names `plugin profile
  migrate <name>`). The `agent_profile_not_found` hint of any other name lists
  `shell` or an installed runtime (`pohunek plugin list`) instead of fixed
  runtime ids.
- New `runtime/runtime_builtin_removed`: resume, fork and every mutation
  (input, attach, resize, rename, metadata) of a session launched from the
  built-in runtime of an official alias this daemon does not compile in
  (pinned, or a record without a launch binding) while no package serves the
  alias. Its hint names the package install and a new session. With the
  package installed, the live session can be used again and resume and fork
  answer `runtime_served_by_package`.
- `pohunekd upgrade-preflight` judges every live session's runtime source with
  the mutation guard, whatever its resume and fork capabilities: a session
  whose runtime resolves nowhere is `would_lose_recovery`
  (`runtime_source_not_served`), and its detail says every interaction is
  refused until the runtime (for a removed built-in, the official package) is
  installed.
- The release catalog lists the Codex package for the Linux x86_64 targets
  only, so a macOS daemon has no source of Codex (#929).
- `integration.status`, `integration.doctor` and `integration.uninstall` that
  name `codex` work without the package: the core-owned Codex handler keeps
  the config home of the earlier built-in, so leftover pohunek hooks are
  reported and removed, in the runtime's home and, with `profile` or
  `all_profiles`, in the homes of host profiles over `codex` (found from each
  profile's `base` and `[env]`, since such a profile does not resolve). Bare
  status and doctor cover only served runtimes.
- A foreground `codex` in a shell session and an unmanaged `codex` are
  recognized only through the installed package's detection manifest and
  `[transcripts]` table; without the package they are not observed as Codex.
- `daemon.doctor` checks embedded manifests only for `shell`, `claude` and
  `hermes`. `pohunek integration --agent` offers `claude` and `hermes` as
  built-in names; `codex` is accepted as a runtime id.
- Documented in `guides/codex-package`, `guides/runtime-packages`,
  `concepts/agent-profiles`, `concepts/sessions`, the
  `runtime-package-recovery` and `update-after-release` runbooks, the source
  map, `docs/install.md`, `docs/architecture.md`, `docs/cli.md` and
  `docs/public-api.md`.
