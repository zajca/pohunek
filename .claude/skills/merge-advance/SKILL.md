---
name: merge-advance
description: >-
  Commit the current milestone unsigned, merge it into main, delete the branch
  and prune its worktree, then write the next NEXT.md and delete the old one.
  Use when the user says "commit bez podpisu merge do main", "udělej commit
  merge do main a připrav nový NEXT.md", "mergni větev do main a větev smaž",
  "commitni všechno a merge do main", or asks to land a milestone and set up the
  next one.
---

# merge-advance — land a milestone and set up the next

Closes out a finished milestone: commit unsigned, merge to `main`, clean up the
branch and worktree, then prepare the next `NEXT.md`. Run this only after the
milestone is implemented and the gates are green (see the `milestone` and
`milestone-review` skills).

## Preconditions

- The gates pass on the branch (run the `gates` skill first if unsure — never
  merge red).
- You are on a milestone branch off `main`, not on `main` itself.

## Steps

1. **Commit — unsigned.** Commit the milestone work with a clean, concise
   message describing the milestone. Commits are NEVER signed: do not add a
   `Co-Authored-By` trailer or any "generated with" footer. Commit in logical
   chunks if the change is large. Exclude transient files the user does not want
   committed (e.g. `NEXT.md`, `idea.md`) when they say so — historically they
   commit "bez docs a next.md a idea.md".

2. **Merge into `main`.** Merge the branch into `main`:

   ```bash
   git switch main
   git merge --no-ff zajca/<milestone-slug>   # or fast-forward if that is the repo's habit
   ```

   Resolve conflicts if any; re-run the `gates` skill on `main` after a
   non-trivial merge.

3. **Delete the branch and prune the worktree.** Everything should now live in
   `main`:

   ```bash
   git worktree remove /tmp/zremoteng-<milestone-slug>   # or .claude/worktrees/<slug>
   git branch -d zajca/<milestone-slug>
   git worktree prune
   ```

   If the user asks to "vyčisti worktrees, všechno by mělo být v main", verify
   with `git worktree list` and reconcile any stragglers.

4. **Write the next `NEXT.md`.** Replace the consumed spec with the next
   milestone's spec: delete the old `NEXT.md` and write a fresh one describing
   the next milestone end to end, with explicit definition-of-done items. Keep it
   complete — no PoC/minimal-version framing unless the user asks. If the next
   milestone is not yet decided, use the `plan-phase` skill instead of guessing.

5. **Report.** State the merge commit, that the branch/worktree are cleaned up,
   and that the new `NEXT.md` is ready (or that planning is needed next).

## Constraints

- Push only when the user asks. This project keeps `main` local unless a push /
  release is requested.
- Never sign commits; no `Co-Authored-By`, no generated-by footer.
- Do not release here — cutting a version is the `release` skill's job.
