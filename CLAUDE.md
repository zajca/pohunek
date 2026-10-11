# Claude Code

Read [AGENTS.md](AGENTS.md) first. It routes tasks to Pohunek's canonical,
scoped instructions; do not maintain a second copy of repository policy here.

- For product boundaries and compatibility, follow
  [.agents/instructions/project.md](.agents/instructions/project.md) and
  [docs/architecture.md](docs/architecture.md).
- For code, tests, and verification, follow
  [.agents/instructions/coding.md](.agents/instructions/coding.md) and
  [.agents/instructions/verification.md](.agents/instructions/verification.md).
- Before any Rust edit, follow
  [.agents/rust-guidelines/SKILL.md](.agents/rust-guidelines/SKILL.md).
- For issue and project writes, follow the
  [github-workflow skill](.agents/skills/github-workflow/SKILL.md).
- Use the relevant milestone, review, gate, handoff, or release skill under
  `.agents/skills/` when its workflow applies. `.claude/skills/` points to those
  canonical files; edit only `.agents/skills/`.

Host-level instructions may add constraints. They do not replace this
repository's security, compatibility, testing, and authorization requirements.
