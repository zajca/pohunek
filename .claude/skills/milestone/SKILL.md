---
name: milestone
description: >-
  Implement the pohunek milestone specified by a GitHub issue in a fresh
  worktree, delegating the build to a subagent or Codex, then run the full
  gate set. Use when the user points at a GitHub issue and asks to implement
  that milestone.
---

# milestone — implement a milestone from its GitHub issue

Implements one milestone specified by a GitHub issue, following the repo's
standing loop: fresh worktree off `main`, guideline-compliant implementation
delegated to parallel workers, then the full gate set green before hand-off.
The issue is the spec — there is no `NEXT.md`.

## Steps

1. **Resolve the issue.** Route through the `github-workflow` skill. Use the
   issue URL/number the user or task explicitly provides; when none is given,
   search for matching issues and reuse a unique open match. If none matches
   and the requested scope is concrete, automatically create the issue with
   its scope and DoD and add it to the configured project. Ask only when the
   scope or competing matches are genuinely ambiguous. Fetch the live issue
   body **and comments** — later comments may hold decisions, follow-ups, and
   evidence that refine the body. Extract the scope and the DoD items with
   their stable IDs — these are the testable success criteria. If they are
   ambiguous, resolve the ambiguity before implementation; do not invent scope.
2. **Create a worktree off `main`.** Use the existing convention:

   ```bash
   git worktree add /tmp/pohunek-<milestone-slug> -b zajca/<milestone-slug> origin/main
   ```

   (Never implement directly on `main`. Do not disturb other checkouts or
   unrelated active worktrees.)
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
7. **Run the gates.** Invoke the `gates` skill. Iterate until every gate is
   green. Report honestly — never claim green without running it.
8. **Record progress in the issue.** Via the `github-workflow` skill, post
   per-DoD-item results with `path:line` evidence and the gate results as
   issue comments as major steps complete; keep the project status consistent
   (In Progress while implementing). When the work stops mid-run (blocked,
   interrupted, or handed over), leave the handoff comment the skill
   specifies: branch/worktree, revision, scope covered vs remaining, the
   exact checks run with their real exit results (including skipped/failed
   ones and why), blockers, and any subagent/worker run IDs.
9. **Report.** Summarize what was implemented against each DoD item with
   `path:line` evidence, and state the gate results. Do not commit or merge here
   — that is the `merge-advance`/`pr-handoff` path.

## Constraints

- No PoC, no minimal/partial versions, no shortcuts unless the user explicitly
  asks. Implement the milestone's full scope.
- No mocks or stubs for specified functionality; if blocked, ask.
- Commit/push only when asked; this skill stops at "implemented + gates green",
  recorded as evidence on the issue.
