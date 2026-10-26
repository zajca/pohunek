# AGENTS.md

Canonical repository entry point. Keep this file short: route each task to the
current authoritative instructions instead of repeating them. When you change a
command, a crate boundary, or a convention, update the guide that owns it (this
file, `docs/development.md`, or a scoped guide) in the same change. Repository text,
code, comments, and commit messages are in English. Security takes priority over
quality, simplicity, and speed. Follow the user's authorized scope; never infer
permission to publish, merge, release, deploy, or use credentials.

For every task, including read-only inspection, diagnosis, and review, never
read `.env*`, key or certificate files, or disclose token values. The detailed
secret-handling policy lives in
[.agents/instructions/coding.md](.agents/instructions/coding.md).

## About this repository

Pohunek is an owner-first control plane for durable coding-agent sessions. The
Rust daemon owns session state, isolated workers own live PTYs, and the CLI and
versioned SDKs are the public client boundary. The core ships no UI; team relay
is additive and not implied to be shipped. See [the authoritative architecture](docs/architecture.md)
and [public API](docs/public-api.md).

## Required instruction routes

Read the linked guide when its trigger applies; a task does not require reading
unrelated runbooks.

- **Every product or architecture change:** [.agents/instructions/project.md](.agents/instructions/project.md)
  for the compatibility window, persisted-state rules, and hard product
  boundaries. `docs/architecture.md` is authoritative when design details differ.
- **Any code, test, documentation, build, or validation change:**
  [.agents/instructions/verification.md](.agents/instructions/verification.md)
  for CI gates, fast loops, testing policy, hermeticity, and evidence requirements.
- **Any implementation, wire-protocol, CLI, or public contract change**
  (including edits to `docs/public-api.md`, `docs/cli.md`, or `docs/knowledge/`):
  [.agents/instructions/coding.md](.agents/instructions/coding.md) for Rust,
  security, configuration, protocol ripple, knowledge, CLI completion, and relay
  constraints.
- **Issue, project, branch, commit, pull request, merge, or release work:**
  [.agents/instructions/workflow.md](.agents/instructions/workflow.md) and the
  [github-workflow skill](.agents/skills/github-workflow/SKILL.md). That skill
  owns issue/project operations; `.github/agent-workflow.json` is its config.
- **A new plugin (runtime package or workflow plugin):** never add it to this
  repository. An official plugin lives in its own repository
  `pohunekd/plugin-<name>`, created from `pohunekd/pohunek-plugin-template` by
  the owner; a third-party plugin lives in its author's account. A plugin
  becomes installable by id through a pull request to
  `pohunekd/pohunek-plugin-index`, never through a change here. Change this
  repository only for a core capability the plugin needs. Naming, required CI,
  compatibility and admission are in
  [Plugin repositories](docs/knowledge/guides/plugin-repositories.md).
- **Rust edits:** before touching `.rs`, read the relevant files named by
  [.agents/rust-guidelines/SKILL.md](.agents/rust-guidelines/SKILL.md), starting
  with `11_universal_guidelines.md`.
- **Gate execution:** use the [gates skill](.agents/skills/gates/SKILL.md),
  which follows the verification guide above.
- **macOS native tests:** the `platform contracts (arm64)` lane runs from one
  nextest archive, and `macOS tier packages (arm64)` runs on main and nightly
  only; both are described in the verification guide's CI tiers section.

For workspace layout and routine development commands, see
[docs/development.md](docs/development.md). The canonical skills live under
`.agents/skills/`; `.claude/skills/` contains discovery links only.
