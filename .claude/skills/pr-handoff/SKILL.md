---
name: pr-handoff
description: >-
  Turn a finished, gate-green milestone branch into an unsigned commit, push
  it, and open a PR whose description is assembled from the milestone's audit
  evidence. Use when the user says "udělej PR", "otevři PR pro tuhle větev",
  "commit a push a vytvoř PR", "make a PR for this branch", "commit and open a
  PR", or after a milestone-build harness run reports all DoD items met with
  the gates green.
---

# pr-handoff — commit, push, open a PR

Bridges the gap between "milestone implemented, gates green" (the `milestone`
and `milestone-review` skills stop there) and a published pull request.
Authorization semantics: a milestone-build harness run whose final audit
reports every DoD item met with all gates green **pre-authorizes** this
handoff — publishing is then part of the autonomous loop, no further ask.
This intentionally supersedes the default "commit/push only when the user
asks" for that one path (see AGENTS.md, "Accepted harness trade-offs"); for
any other branch, wait for the user to request it.

## Preconditions

- The **full CI-mirror gate set** passes on the branch. For a harness
  milestone that is the gate set in `.lh-harness/workflows/milestone-build.md`
  (already run by the milestone's audit); for a plain branch, run that same
  set by hand. The `gates` skill is a narrower subset — no Hermes
  compatibility, web, or real-daemon gates — and is never sufficient on its
  own to authorize a handoff.
- You are on a milestone branch off `main`, not on `main` itself.
- The milestone's verification evidence is available: gate results, DoD
  verdicts with `path:line` proof, test counts. From a harness run, take them
  from the final auditor report and the final response episode.

## Steps

1. **Stage explicitly.** Add files by path — never `git add -A`. Exclude
   transient files the user does not want committed (historically `NEXT.md`,
   `idea.md`, harness run state).

2. **Commit — unsigned.** Use `git commit --no-gpg-sign`; commits in this
   repo are NEVER signed, and `--no-gpg-sign` keeps that true even on
   machines whose global git config forces a signer. Message: concise,
   imperative, English. Never add a `Co-Authored-By` trailer or any
   "generated with" footer.

3. **Push.** `git push -u origin <branch>`.

4. **Build the PR description from evidence, not memory.** English body,
   before/after structure:
   - *Summary* — what changed and why, one paragraph; link the tracked issue
     with `Closes #N` when one exists.
   - *Before / After* — what was missing, what is observable now.
   - *Verification* — the gate results, per-item DoD verdicts with
     `path:line` evidence, test counts, and any verification step worth
     repeating by hand. This section mirrors the final audit report; do not
     invent or embellish results.

5. **Open and verify.** `gh` in a non-interactive agent shell never prompts:
   save the assembled title and body to a scratch file and pass them
   explicitly — `gh pr create --base main --head <branch> --title "<title>"
   --body-file <file>` (remove the scratch file afterwards). Confirm the URL,
   then check the initial CI status (`gh pr checks <n>`). Report the PR
   number, URL, and CI state.

## Constraints

- Do not merge here — merging is the `merge-advance` skill or the user's PR
  flow on GitHub.
- Do not release, tag, or delete branches.
- PR text only references secrets-free evidence; never quote environment
  values, tokens, or logs that could contain them.
