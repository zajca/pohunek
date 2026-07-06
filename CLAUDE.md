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

## Milestone workflow skills

The milestone loop described in AGENTS.md (`plan → implement in a worktree →
review against NEXT.md → merge and advance → release`) is encoded as skills under
`.claude/skills/`. Prefer them over re-deriving the steps each time; they auto-
trigger from the usual phrasing:

- **`plan-phase`** — plan the next phase interactively (one open question at a
  time) and write a complete end-to-end `NEXT.md`.
- **`milestone`** — implement the next milestone from `NEXT.md` in a fresh
  worktree, then run the gates.
- **`milestone-review`** — review a branch/worktree against `NEXT.md`'s DoD with
  `path:line` evidence, delegate fixes, re-run the gates.
- **`merge-advance`** — commit unsigned, merge to `main`, prune the
  branch/worktree, write the next `NEXT.md`.
- **`release`** — cut a version with `scripts/release` and verify the Release
  workflow publishes the glibc + MUSL x86_64 binaries.
- **`gates`** — the shared verification block (fmt / clippy `-D warnings` / test
  / release build / `cargo xtask docs check`); the other skills call it.

## Keep the assistant knowledge bundle current

`docs/knowledge/` is the hand-authored source for the Universal Pohunek
Assistant. Per AGENTS.md, treat it as part of the change, never a follow-up:
whenever you touch something it describes — a CLI command/flag, a protocol
method/event, GUI behavior, an operating-model concept, a safety rule, the
`docs/public-api.md` surface, or a path in
`docs/knowledge/assistant/source-map.md` — update the matching knowledge file in
the *same* change and re-run `cargo xtask docs check`. For wire-protocol work
this is one more ripple target alongside `client`/`daemon`/`cli`/`gui-core`: a
new method/event is not done until the bundle and `docs/public-api.md` reflect
it. If unsure whether a change is assistant-visible, check whether any file under
`docs/knowledge/` mentions the surface you changed.

## Agent teams and sub-agents

This is a layered Rust workspace where reviews and features benefit from
parallel exploration. Good fits here:

- **Pre-PR / code review:** `security-reviewer` (secret handling, the `sh -c`
  attach surface, NetBird trust boundary) + `silent-failure-hunter` (swallowed
  errors in the daemon/state machines) + `performance-reviewer`, synthesized by
  `product-engineer`.
- **Protocol changes:** spawn parallel implementers per affected crate
  (`protocol` → `client`/`daemon`/`cli`/`gui-core`), coordinated by `tech-lead`,
  because one wire change ripples across crates — and out into
  `docs/public-api.md` and the `docs/knowledge/` bundle (see "Keep the assistant
  knowledge bundle current").
- **State-machine bugs:** competing-hypothesis investigation across
  `gui-core`/`daemon` session detection.

By default, delegate milestone implementation and post-review fixes to parallel
subagents or Codex — this is the standing mode of work here, not something to
wait for permission on. Brief every sub-agent with concrete `path:line` context
(per your global briefing protocol); they start with a clean context window.

## Project memory

Cross-session memory for this project lives under
`~/.claude/projects/-home-zajca-Code-me-zremoteng/memory/` (index: `MEMORY.md`).
Relevant standing facts: pohunek is experimental with no back-compat; the GUI is
the pinned native control-plane direction; the `ms-rust` skill must precede Rust
edits. Consult it and keep it current.
