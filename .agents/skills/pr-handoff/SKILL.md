---
name: pr-handoff
description: >-
  Turn a finished, gate-green milestone into a stack of small sequential PRs
  (one per slice branch, unsigned commits, bottom-up), each described from the
  milestone's audit evidence, then update the milestone's issue. Use when the
  user asks for a PR for a branch, or after a milestone-build harness run
  reports all DoD items met with the gates green.
---

# pr-handoff — commit, push, open a PR

Bridges the gap between "milestone implemented, gates green" (the `milestone`
and `milestone-review` skills stop there) and a published pull request.
Authorization semantics: a milestone-build harness run whose final audit
reports every DoD item met with all gates green **pre-authorizes** this
handoff — publishing is then part of the autonomous loop, no further ask.
This intentionally supersedes the default "commit/push only when the user
asks" for that one path (see AGENTS.md, "Accepted harness trade-offs"); for
any other branch, wait for the user to request it. This preauthorization is
for publishing only — it does not authorize merging the PR or anything beyond
it.

## Preconditions

- The **full CI-mirror gate set** passes on the branch. For a harness
  milestone that is the gate set in `.lh-harness/workflows/milestone-build.md`
  (already run by the milestone's audit); for a plain branch, run that same
  set by hand. Follow the `gates` skill together with the authoritative
  AGENTS.md commands, including Hermes compatibility, SDK workspace, and real-daemon
  checks. A partial run never authorizes this handoff.
- You are on a milestone slice branch, not on `main` itself. The bottom
  slice is based on `main`; every other slice is based on the slice below it
  (see the `milestone` skill and `pullRequests` in
  `.github/agent-workflow.json`).
- The milestone's GitHub issue is known (explicit URL/number from the user or
  task; otherwise resolve per the `github-workflow` skill — unique
  unambiguous match only, otherwise ask).
- The milestone's verification evidence is available: gate results, DoD
  verdicts with `path:line` proof, test counts. From a harness run, take them
  from the final auditor report and the final response episode.

## One PR per slice — the stack

Publish the milestone as a stack of small sequential PRs, one per slice,
bottom-up. For each slice, from the bottom:

- Its gate set passes on that slice branch alone; a later slice never
  excuses a red lower one.
- `--base` is `main` for the bottom slice and the previous slice's branch for
  every other; `--head` is the slice's own branch. The diff each PR shows is
  therefore only its own slice.
- The description names the stack position and neighbours (`Stack 2/3:
  #<prev> → **this** → #<next>`, filling in #<next> once it exists) and the
  DoD items this slice satisfies.
- Non-final PRs reference the issue with `Refs #N`; only the final PR uses
  `Closes #N`, so the issue cannot close before the whole stack lands.
- A review fix to a lower slice is committed on that slice's branch; restack
  the branches above it with `git rebase --update-refs` from the top branch,
  re-run the applicable gates on every rebased slice (its inputs changed with
  its base), and keep those higher branches local. Push the changed lower
  branch first, wait for its current head's CI and review, then publish higher
  branches one at a time, bottom-up, using `--force-with-lease` for rewritten
  remote heads. Do not batch-push the restacked branches.

## GitHub Actions capacity

The repository has limited concurrent Actions capacity. Coordinate with other
agents publishing independent PRs: check active PR runs before a head push and
avoid starting another expensive run while capacity is saturated. Never cancel
another agent's run just to make room. For a stack, commit and verify every
slice locally first, then push and open only the bottom ready PR. Publish the
next slice after the previous PR's current-head CI and review finish; a local
rebase of a higher slice does not need a push yet. Record unpublished slices as
pending in issue tracking, then add each PR link when that slice is published.

Before replacing a published PR head, record its active `pull_request` run IDs
(`gh run list --branch <slice-branch> --event pull_request --json
databaseId,headBranch,status,url`) and confirm each belongs to that PR. If a
run has not completed, cancel it with `gh run cancel <run-id>` before the new
push or immediately afterward. Read its status again with `gh run view
<run-id> --json status,conclusion`; a cancel request alone is not confirmation.
Confirm it completed with `cancelled` (or had completed before cancellation).
The CI workflow's same-ref `cancel-in-progress` is a fallback, not a reason to
leave an obsolete run consuming capacity. Watch only the new head after the
push.

A milestone that is one small concern is a stack of one: a single PR on
`main` with `Closes #N`.

## Steps

Run steps 1–5 for each slice, bottom-up, observing the capacity rule above;
repeat step 6 after each publication to record its PR link and the remaining
unpublished slices as pending.

1. **Stage explicitly.** Add files by path — never `git add -A`. Exclude
   transient files the user does not want committed (historically `idea.md`,
   harness run state).
2. **Commit — unsigned.** Use `git commit --no-gpg-sign`; commits in this
   repo are NEVER signed, and `--no-gpg-sign` keeps that true even on
   machines whose global git config forces a signer. Message: concise,
   imperative, English. Never add a `Co-Authored-By` trailer or any
   "generated with" footer.
3. **Push.** `git push -u origin <slice-branch>` only when that slice is next
   to receive CI and review. Keep the other slices local until their turn.
4. **Build the PR description from evidence, not memory.** English body,
   before/after structure:
   - *Summary* — what changed and why, one paragraph; link the tracked issue
     with `Closes #N` on the final PR of the stack, `Refs #N` on the others;
     state the stack position and the DoD items this slice covers.
   - *Before / After* — what was missing, what is observable now.
   - *Verification* — the gate results, per-item DoD verdicts with
     `path:line` evidence, test counts, and any verification step worth
     repeating by hand. This section mirrors the final audit report; do not
     invent or embellish results.
5. **Open and verify.** `gh` in a non-interactive agent shell never prompts:
   save the assembled title and body to a scratch file and pass them
   explicitly — `gh pr create --base <main-or-previous-slice-branch> --head
   <slice-branch> --title "<title>" --body-file <file>` (remove the scratch
   file afterwards). Confirm the URL, then check the initial CI status (`gh pr
   checks <n>`). Report the PR number, URL, and CI state.

   **Review trigger.** The automated review runs only for a PR that carries
   the `ai:review` label, so add it right after opening (`gh pr edit <n>
   --add-label ai:review`; create the label first with `gh label create
   ai:review` if the repository does not have it yet) and verify it from
   `gh pr view <n> --json labels`. Re-apply it after every push to the PR
   when the reviewer removed it, and never assume a review is coming for an
   unlabeled PR.
6. **Update the milestone tracking.** Via the `github-workflow` skill, comment
   the ordered list of stack PR links on the milestone issue with the standard
   handoff content (branch/worktree, HEAD revision, scope covered, exact
   checks run with real exit results including any skipped/failed ones,
   remaining work/blockers, and worker run IDs from the harness run), and keep
   the project status `In Progress` — a PR opening is never `Done`. `Done`
   happens when the work has verifiably landed on the remote default branch
   (via `merge-advance` or the PR being merged); verify every write from the
   API response.

## Constraints

- Do not merge here — merging is the `merge-advance` skill or the user's PR
  flow on GitHub, bottom-up. The repository does not delete head branches on
  merge, so after a lower PR merges retarget the next one explicitly
  (`gh pr edit <n> --base main`); if the lower PR was squash- or
  rebase-merged, rebase the next slice onto `origin/main` first
  (`git rebase --onto origin/main <old-lower-branch> <slice-branch>`).
- Do not release, tag, or delete branches.
- Issue/project writes follow the `github-workflow` skill's safe-persistence
  rules; a blocked sync is reported as blocked, never claimed successful.
- PR text only references secrets-free evidence; never quote environment
  values, tokens, or logs that could contain them.
