# CLAUDE.md

Project guidance for Claude Code. **Read [AGENTS.md](AGENTS.md) first** — it is
the canonical guide (what pohunek is, repo map, build/test/lint gates,
conventions, workflow). This file only adds Claude-specific notes; it does not
repeat AGENTS.md.

Your personal global instructions in `~/.claude/CLAUDE.md` also apply and take
precedence where they are stricter (Czech communication, English files,
security > quality > simplicity > time, no mocks, no hardcoded values).

## Mandatory: read the Rust guidelines before any Rust edit

Before creating or modifying **any** `.rs` file — even a one-line change — read
the guideline files relevant to the task from **`.agents/rust-guidelines/`**
(vendored in-repo, no external checkout needed). Use
`.agents/rust-guidelines/SKILL.md` as the which-file-when index; at minimum read
`11_universal_guidelines.md`, adding correctness/application/library files as the
task warrants. Apply `M-CANONICAL-DOCS` and the rest, and update the
`// Rust guideline compliant <date>` marker when a file is fully compliant. This
is non-negotiable for this repo.

If you have the `ms-rust` Agent Skill registered locally (e.g. symlinked into
`~/.claude/skills/`), you may invoke it instead — it wraps the same guidelines.
But the vendored copy is the source of truth here so the rule holds for every
agent and machine, not just a personally-configured one.

## Verifying work

CI is the source of truth. Before reporting a Rust change as done, run the gate
set from AGENTS.md ("Build, test, lint") — clippy is `-D warnings`, so a warning
is a failure. For docs/knowledge changes, also run `cargo xtask docs check`.

## Agent teams and sub-agents

This is a layered Rust workspace where reviews and features benefit from
parallel exploration. Good fits here:

- **Pre-PR / code review:** `security-reviewer` (secret handling, the `sh -c`
  attach surface, NetBird trust boundary) + `silent-failure-hunter` (swallowed
  errors in the daemon/state machines) + `performance-reviewer`, synthesized by
  `product-engineer`.
- **Protocol changes:** spawn parallel implementers per affected crate
  (`protocol` → `client`/`daemon`/`cli`/`gui-core`), coordinated by `tech-lead`,
  because one wire change ripples across crates.
- **State-machine bugs:** competing-hypothesis investigation across
  `gui-core`/`daemon` session detection.

Brief every sub-agent with concrete `path:line` context (per your global
briefing protocol); they start with a clean context window.

## Project memory

Cross-session memory for this project lives under
`~/.claude/projects/-home-zajca-Code-me-zremoteng/memory/` (index: `MEMORY.md`).
Relevant standing facts: pohunek is experimental with no back-compat; the GUI is
the pinned native control-plane direction; the `ms-rust` skill must precede Rust
edits. Consult it and keep it current.
