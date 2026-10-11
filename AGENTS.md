# AGENTS.md

Canonical repository entry point. Keep this file short: route each task to the
current authoritative instructions instead of repeating them. Repository text,
code, comments, and commit messages are in English. Security takes priority over
quality, simplicity, and speed. Follow the user's authorized scope; never infer
permission to publish, merge, release, deploy, or use credentials.

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
- **Any implementation or generated contract change:**
  [.agents/instructions/coding.md](.agents/instructions/coding.md) for Rust,
  security, configuration, knowledge, CLI completion, and relay constraints.
- **Issue, project, branch, commit, pull request, merge, or release work:**
  [.agents/instructions/workflow.md](.agents/instructions/workflow.md) and the
  [github-workflow skill](.agents/skills/github-workflow/SKILL.md). That skill
  owns issue/project operations; `.github/agent-workflow.json` is its config.
- **Rust edits:** before touching `.rs`, read the relevant files named by
  [.agents/rust-guidelines/SKILL.md](.agents/rust-guidelines/SKILL.md), starting
  with `11_universal_guidelines.md`.
- **Gate execution:** use the [gates skill](.agents/skills/gates/SKILL.md),
  which follows the verification guide above.

For workspace layout and routine development commands, see
[docs/development.md](docs/development.md). The canonical skills live under
`.agents/skills/`; `.claude/skills/` contains discovery links only.
