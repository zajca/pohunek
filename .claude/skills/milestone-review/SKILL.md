---
name: milestone-review
description: >-
  Read-only review of a branch/worktree against its GitHub issue's
  definition-of-done, reporting each item with path:line evidence, then
  delegating any discrepancy fixes to a subagent/Codex and re-running the
  gates. Use when the user asks for a detailed review of a branch or to verify
  a milestone matches its spec.
---

# milestone-review — verify a branch against its issue

Reviews an implemented milestone branch/worktree against the definition-of-done
recorded on its GitHub issue, then drives fixes for any gap. This is the review
half of the milestone loop: it does not implement scope itself, it verifies
scope and delegates corrections. Findings and evidence are persisted on the
issue via the `github-workflow` skill.

## Inputs

- The GitHub issue specifying the milestone (explicit URL/number from the
  user or task; otherwise infer per the `github-workflow` skill's
  resolution rules — unique unambiguous match only, otherwise ask).
- The branch or worktree to review (e.g. `zajca/milestone-4-attach-stream`, or
  a path like `/tmp/pohunek-milestone-3-pty-sessions`).

## Steps

1. **Load the DoD.** Fetch the live issue body and comments (comments may
   refine scope or record decisions) and enumerate every definition-of-done
   item with its stable ID. These are the checklist you review against —
   nothing more, nothing less.
2. **Review read-only, item by item.** Check out or `cd` into the branch/worktree
   and verify each DoD item is actually implemented. For every item record a
   verdict (met / partial / missing) with concrete `path:line` evidence. Read the
   applicable `.agents/rust-guidelines/` files so review comments match the
   repo's conventions (typed errors, no silent defaults, documented magic values,
   `M-CANONICAL-DOCS`, tests for new logic). For a deeper pass this maps well to
   parallel specialist reviewers (security-reviewer for the `sh -c` attach
   surface and secret handling, silent-failure-hunter for swallowed daemon
   errors, performance-reviewer), synthesized before you report.
3. **Record findings on the issue.** Post the per-item verdict table with
   `path:line` evidence and the confirmed gaps to the issue as a comment (via
   the `github-workflow` skill). Keep the project status at `In Progress`
   while gaps are open; blockers and review state live in comments.
4. **Delegate the fixes.** For each confirmed discrepancy, hand the fix to a
   subagent or Codex (this is the standing default). Brief each with concrete
   `path:line` context and the exact DoD item it must satisfy. Do not silently
   fix and re-review in one blur — keep the review findings and the fix work
   traceable on the issue. Verified out-of-scope findings discovered during
   the review go to their own follow-up issues (dedup via the
   `github-workflow` skill); an unmet original DoD item is never moved to a
   follow-up to claim the milestone done.
5. **Re-run the gates after fixes.** Invoke the `gates` skill on the branch
   once fixes land. Iterate until green.
6. **Final verdict.** Walk each DoD item with its final status and `path:line`
   evidence, and state the gate results. Say plainly whether the milestone
   matches its issue's specification, and record that final verdict as an
   issue comment too.

## Constraints

- The review pass itself is read-only; changes happen only through delegated fix
  work, then re-verification.
- Verify effects — do not report a discrepancy as fixed without re-reading the
  code and re-running the affected gate.
- Do not merge here; merging and advancing is the `merge-advance` skill's job.
- Follow the `github-workflow` skill's safe-persistence rules for all reads
  and writes to the issue/project; do not close the issue or set `Done` here.
