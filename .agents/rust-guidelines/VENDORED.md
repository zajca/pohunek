# Vendored Rust guidelines (ms-rust)

These files are vendored into the repository so **every** agent (Claude Code,
Codex, others), every clone, and CI can read them from a stable in-repo path
without needing an external checkout. Agents should read the relevant files
directly from this directory — see `AGENTS.md` for when to use which file, and
`SKILL.md` here for the per-file routing index.

## Source

- Upstream: <https://gitlab.com/lx-industries/ms-rust-skill>
  (`git@gitlab.com:lx-industries/ms-rust-skill.git`)
- Vendored at commit: `e715a1c4df0c188951b0665c9bcd214ac826d6af` (2026-06-19)
- Upstream packages Microsoft's
  [Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/guidelines/index.html)
  as an [Agent Skill](https://agentskills.io/).

## License and attribution

The guideline text (`01_*.md` … `15_*.md`) is copyright © Microsoft Corporation
and licensed under the MIT license. The skill packaging (`SKILL.md`, `README.md`,
file structure) is by lx-industries, also MIT. See `NOTICE` in this directory.

## Re-syncing

This is a point-in-time copy. To update, refresh from upstream and bump the
commit hash above:

```bash
git clone git@gitlab.com:lx-industries/ms-rust-skill.git /tmp/ms-rust-skill
cd /tmp/ms-rust-skill && uv run generate.py
cp /tmp/ms-rust-skill/{0,1}*.md /tmp/ms-rust-skill/SKILL.md /tmp/ms-rust-skill/README.md \
   <repo>/.agents/rust-guidelines/
```

Do not hand-edit the guideline files in place; edits would be lost on the next
sync. Project-specific deviations belong in `AGENTS.md`.
