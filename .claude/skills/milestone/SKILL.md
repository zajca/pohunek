---
name: milestone
description: >-
  Implement the next pohunek milestone from NEXT.md in a fresh worktree,
  delegating the build to parallel subagents/Codex, then run the full gate set.
  Use when the user says "naimplementuj další milestone", "udělej si worktree a
  podle NEXT.md naimplementuj milestone", "implement the next milestone", "začni
  na milestone X", "podle NEXT.md udělej další fázi", or points at NEXT.md and
  asks to build it.
---

# milestone — implement the next milestone

Implements one milestone described in `NEXT.md`, following the repo's standing
loop: fresh worktree off `main`, guideline-compliant implementation delegated to
parallel workers, then the full gate set green before hand-off.

`NEXT.md` is the transient per-milestone spec (uncommitted, deleted after merge).
Treat its definition-of-done items as the success criteria for this run.

## Steps

1. **Read the spec.** Read `NEXT.md` at the repo root. Extract the milestone's
   scope and its DoD items — these are the testable success criteria. If
   `NEXT.md` is missing or ambiguous, stop and ask; do not invent scope.

2. **Create a worktree off `main`.** Use the existing convention — a worktree
   per milestone, branched off `main`:

   ```bash
   git worktree add /tmp/zremoteng-<milestone-slug> -b zajca/<milestone-slug> origin/main
   ```

   (Some past milestones used `.claude/worktrees/<slug>` — either location is
   fine; match whatever the user names. Never implement directly on `main`.)

3. **Read the Rust guidelines first — mandatory.** Before creating or modifying
   ANY `.rs` file, read the applicable files from `.agents/rust-guidelines/`.
   Use `.agents/rust-guidelines/SKILL.md` as the which-file-when index; at
   minimum `11_universal_guidelines.md`, adding `02_application_*`,
   `03_correctness_*`, and `06/12/13/14/15` (library design) as the task
   warrants. Apply `M-CANONICAL-DOCS`, short names, documented magic values,
   `#[expect(..., reason = "...")]` over `#[allow]`. Update the
   `// Rust guideline compliant <date>` marker on any file you bring fully into
   compliance.

4. **Implement via parallel subagents/Codex by default.** Decompose the
   milestone and delegate implementation to parallel subagents or Codex — this
   is the default, not something to wait for permission on. Brief each worker
   with concrete `path:line` context (per the global briefing protocol); they
   start with a clean context window. If the wire protocol
   (`crates/protocol`) changes, expect ripples in `client`, `daemon`, `cli`, and
   `gui-core` — update and test all of them, plus `docs/public-api.md`.

5. **Write tests for all new logic.** Unit tests inline (`#[cfg(test)]`) for
   private behavior; `tests/` for integration. Extend the existing
   protocol/state-machine suites rather than adding untested branches.

6. **Keep the assistant knowledge bundle current.** If the milestone changes a
   CLI command/flag, a protocol method/event, GUI behavior, an operating-model
   concept, a safety rule, `docs/public-api.md`, or a path in
   `docs/knowledge/assistant/source-map.md`, update the matching
   `docs/knowledge/` file in the *same* change. A stale bundle is treated like
   stale code.

7. **Run the gates.** Invoke the `gates` skill (fmt / clippy -D warnings / test
   / release build / `cargo xtask docs check`). Iterate until every gate is
   green. Report honestly — never claim green without running it.

8. **Report.** Summarize what was implemented against each DoD item with
   `path:line` evidence, and state the gate results. Do not commit or merge here
   — that is the `merge-advance` skill's job.

## Constraints

- No PoC, no minimal/partial versions, no shortcuts unless the user explicitly
  asks. Implement the milestone's full scope.
- No mocks or stubs for specified functionality; if blocked, ask.
- Commit/push only when asked; this skill stops at "implemented + gates green".
