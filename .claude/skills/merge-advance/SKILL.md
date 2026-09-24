---
name: merge-advance
description: >-
  Commit the current milestone unsigned, merge it into main, delete the branch
  and prune its worktree, then update the milestone's issue and project status.
  Use when the user asks to land a finished milestone locally.
---

# merge-advance — land a milestone locally and update tracking

Closes out a finished milestone: commit unsigned, merge to `main`, clean up
the branch and worktree, then record the landing on the milestone's issue and
project. Run this only after the milestone is implemented and the gates are
green (see the `milestone` and `milestone-review` skills).

## Preconditions

- The gates pass on the branch (run the `gates` skill first if unsure — never
  merge red).
- You are on a milestone branch off `main`, not on `main` itself.
- The milestone's GitHub issue is known (explicit URL/number from the user or
  task; otherwise resolve per the `github-workflow` skill — unique
  unambiguous match only, otherwise ask).

## Steps

1. **Commit — unsigned.** Commit the milestone work with a clean, concise
   message describing the milestone. Commits are NEVER signed: do not add a
   `Co-Authored-By` trailer or any "generated with" footer. Commit in logical
   chunks if the change is large. Exclude transient files the user does not
   want committed (e.g. `idea.md`, harness run state) when they say so.
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
   git worktree remove /tmp/pohunek-<milestone-slug>
   git branch -d zajca/<milestone-slug>
   git worktree prune
   ```

   If the user asks to "vyčisti worktrees, všechno by mělo být v main", verify
   with `git worktree list` and reconcile any stragglers.
4. **Record the landing on the issue.** Via the `github-workflow` skill, post
   a comment with the merge commit and the final gate evidence. A local merge
   without an authorized push is not a landing for any repository change
   (code, scripts, docs, skills, config): keep the issue open at `In
   Progress` and record the local merge commit as evidence instead of closing
   it. Close the issue as completed and set the project to `Done` only when
   the landing on the remote default branch is verified (and its DoD met).
   Verified out-of-scope follow-ups discovered during the milestone go to
   their own issues, never into this one's scope; never move an unmet original
   DoD item to a follow-up to claim the milestone done. Verify every write
   from the API response.
5. **Set up the next work.** There is no next `NEXT.md`. If the next
   milestone is not yet planned, hand over to the `plan-phase` skill (its plan
   lands as a GitHub issue) instead of guessing.

6. **Report.** State the merge commit, that the branch/worktree are cleaned
   up, and the issue/project updates (verified landed).

## Constraints

- Push only when the user asks. This project keeps `main` local unless a push /
  release is requested.
- Never sign commits; no `Co-Authored-By`, no generated-by footer.
- Issue/project writes follow the `github-workflow` skill's safe-persistence
  rules; permission or network errors block the sync and are reported
  honestly, never papered over.
- Do not release here — cutting a version is the `release` skill's job.
