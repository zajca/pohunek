---
name: milestone-review
description: >-
  Read-only review of a branch/worktree against its NEXT.md definition-of-done,
  reporting each item with path:line evidence, then delegating any discrepancy
  fixes to a subagent/Codex and re-running the gates. Use when the user says
  "udělej detailní code review ověř že je NEXT.md správně naimplementováno ve
  větvi X", "udělej důkladné review větve X", "ověř že je milestone hotový",
  "review this branch against NEXT.md", or asks to verify a milestone matches
  its spec.
---

# milestone-review — verify a branch against NEXT.md

Reviews an implemented milestone branch/worktree against its `NEXT.md`
definition-of-done, then drives fixes for any gap. This is the review half of
the milestone loop: it does not implement scope itself, it verifies scope and
delegates corrections.

## Inputs

- The branch or worktree to review (e.g. `zajca/milestone-4-attach-stream`, or a
  path like `/tmp/zremoteng-milestone-3-pty-sessions`).
- The `NEXT.md` that specifies the milestone (usually at the branch's repo root,
  or the current one if the branch is checked out here).

## Steps

1. **Load the DoD.** Read `NEXT.md` and enumerate every definition-of-done item.
   These are the checklist you review against — nothing more, nothing less.

2. **Review read-only, item by item.** Check out or `cd` into the branch/worktree
   and verify each DoD item is actually implemented. For every item record a
   verdict (met / partial / missing) with concrete `path:line` evidence. Read the
   applicable `.agents/rust-guidelines/` files so review comments match the
   repo's conventions (typed errors, no silent defaults, documented magic values,
   `M-CANONICAL-DOCS`, tests for new logic). For a deeper pass this maps well to
   parallel specialist reviewers (security-reviewer for the `sh -c` attach
   surface and secret handling, silent-failure-hunter for swallowed daemon
   errors, performance-reviewer), synthesized before you report.

3. **Report discrepancies precisely.** Produce a list of confirmed gaps: DoD
   item, what is wrong, `path:line`, and what "correct" looks like. Distinguish a
   genuine DoD miss from a nice-to-have.

4. **Delegate the fixes.** For each confirmed discrepancy, hand the fix to a
   subagent or Codex (this is the standing default). Brief each with concrete
   `path:line` context and the exact DoD item it must satisfy. Do not silently
   fix and re-review in one blur — keep the review findings and the fix work
   traceable.

5. **Re-run the gates after fixes.** Invoke the `gates` skill (fmt / clippy
   -D warnings / test / release build / `cargo xtask docs check`) on the branch
   once fixes land. Iterate until green.

6. **Final verdict.** Walk each DoD item with its final status and `path:line`
   evidence, and state the gate results. Say plainly whether the milestone
   matches `NEXT.md` or still has open gaps.

## Constraints

- The review pass itself is read-only; changes happen only through delegated fix
  work, then re-verification.
- Verify effects — do not report a discrepancy as fixed without re-reading the
  code and re-running the affected gate.
- Do not merge here; merging and advancing to the next milestone is the
  `merge-advance` skill's job.
