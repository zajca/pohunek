---
name: coordinate
description: >-
  Run a milestone or patch release as the coordinator of a team of pohunek
  agent sessions: dispatch issues, review every PR head, enforce the merge
  policy, keep CI and the agents moving, and drive the release to a published,
  verified tag. Use when the owner asks you to coordinate, manage, or finish a
  milestone or release with several agents rather than implement it yourself.
---

# coordinate — run a milestone or release with a team of agents

The coordinator does not program. It decides within the owner's
authorizations, keeps every worker busy on the critical path, verifies every
claim against the system of record, and gets the release published. Workers
implement through the `milestone`, `deliver-issue`, `gates` and `pr-handoff`
skills; the release itself goes through the `release` skill. This skill is the
loop around them.

## Role and boundaries

- **Coordinate, do not implement.** Your tools are cross-session messages,
  `pohunek session screen|input` (`--host <name>` for a remote host), `gh`
  queries, issue/PR comments, milestone edits, and read-only review subagents.
  Code changes go to a worker with a brief.
- **Never block on questions.** Decide within the authorizations you have and
  keep the work moving. Ask the owner only for a release, an irreversible step,
  or a product decision that is genuinely theirs; then give options with a
  recommendation in plain text, and keep the team working while you wait.
- **Releases and tags need the owner's explicit approval** for that version.
  Never infer it. A release agent may require the owner's confirmation in its
  own session; relay the owner's exact words, or, when the owner tells you to,
  type the confirmation into that session labelled as typed by you on their
  instruction.
- **Verify effects, never intent.** Before you report a merge, a closed issue,
  a green run, a tag or a published release, re-query GitHub or the host and
  confirm it. Every status line to the owner names the SHA, run id or URL.
- **Security constraints apply to every agent:** no secret values in any
  output, brief, scratch file or comment; PR bodies use only `Refs #N`; no
  `Co-Authored-By`; kill processes only by exact, verified PID, never by name
  or pattern; never touch the owner's primary checkout or agent homes on a
  host. If a secret reaches your context, stop, tell the owner, recommend
  rotation.

## Setup

1. **Shared context file** `/tmp/agent-context-<milestone>.md`: roster and
   ownership, the merge policy below, intake rules, dispatch rules, and one
   `CURRENT:` line you rewrite at every state change (freeze on/off, release
   candidate SHA, broken tooling such as a down review bot). Point every brief
   at it.
2. **Roster.** One worker per open issue or concern; one CI/release owner; a
   worker on a remote host (for example macOS) for platform evidence.
3. **Watchers** (each as a Monitor or a background loop that exits on the
   event; re-arm on expiry):
   - a CI fail-fast watcher that cancels a PR or dispatch run with a real
     failed job (queued and in-progress runs, skip a branch on request);
   - a state watcher for the PRs on the critical path (head, checks, dispatch,
     `main`), emitting only changes;
   - an idle subscription on each worker (one-shot; re-subscribe after each
     notice);
   - a context watcher (see Agent supervision).

## Intake control

When tasks are created faster than they close, freeze intake: only red-CI
flakes and high-severity regressions of the milestone enter, and only with
your approval. Review leftovers go to one next-milestone checklist issue per
PR. Move anything else to the next milestone with a one-line reason.

## Work loop

1. **Brief** every worker with the four-block protocol (what you know, where
   to start, what to do, constraints). For risky changes (recovery, trust,
   persisted state, packaging) require a written design first and review it
   before any code: decide its open questions explicitly.
2. **Push discipline:** local gates and a local Codex review before every
   push; one push per review round; no pushes while CI runs unless you ask.
3. **Review every PR head yourself** when the review bot is unavailable or
   the change is on the release path: run two to four read-only reviewer
   subagents in parallel, one per commit or per concern, with a shared
   `/tmp/agent-context-review-<pr>.md` and a confidence threshold of 80.
   Post one comment naming the exact head and the verdict. After fixes,
   review only the delta (`git range-diff` after a rebase). A LOW finding that
   would cost another CI round may be deferred to a filed issue; say so in the
   comment.
4. **Batch** findings from all reviewers into one fix push. Prefer one PR with
   ordered commits over a stacked train when the release clock matters, and
   review it commit by commit.

## Merge policy

- Merge when the PR CI is green at the exact head, a valid review names that
  head, and the tier=main dispatch is green where the dispatch rules require
  one (release workflows, packaging, SDK, dependencies, protocol window).
- **Semantic check:** if `main` moved since the PR's CI base, `cargo check
  --workspace --all-targets --all-features` plus fmt on the PR merged with
  `main` before merging.
- **Atomic merge:** fetch, compare `origin/main` with the checked base, and
  `gh pr merge --match-head-commit <head>` in one command that refuses when
  `main` moved.
- Close the issue on merge with the PR CI run id (and dispatch id).
- On the release clock, fire the dispatch in parallel with PR CI and cancel
  the other on a real failure. A docs-only delta after a green dispatch keeps
  that dispatch as evidence (verify the delta with `git diff --name-only`).
- Merge without pausing for a `main` run: pushes to `main` drain, so every
  started main run completes and reports a verdict (#1209). A red `main` run
  is top priority.

## CI and GitHub hygiene

- Cancel superseded runs on the critical path's runners when their failures
  are already known. Never cancel a `main` run only because a newer merge
  landed: the drain depends on each started main run completing.
- A transient registry stall (an optional npm platform package skipped, a
  pinned sha mismatch right after a stalled `npm ci`) may be rerun with
  `gh run rerun --failed` at most twice; anything else stops for a diagnosis.
- **Rate limits:** repeated 403s mean someone is polling hard. Find the
  process (`ps` for `gh run|pr|api` loops), verify its command line and age,
  and stop a clearly runaway loop by its exact PID; ask workers to poll at
  most every 5 minutes. `gh api rate_limit` can understate an exhausted
  primary quota.

## Agent supervision

- After every `pohunek session input`, re-read the screen 20–60 seconds later:
  the agent may show a menu or a question that needs an answer. Keep remote
  inputs under about 400 characters; split longer ones.
- When an agent goes idle, confirm it has a watcher that wakes it on its next
  event; otherwise give it the next step at once.
- **Context:** every 15 minutes read each worker's `ctx … N%` bar from
  `pohunek session screen`; when an idle worker is at about 40% or more, send
  `/compact` (at most once per 20 minutes per worker). Report a failed
  compaction (for example an exhausted account limit) once, not in a loop.

## Release

1. **Merge freeze** from the last milestone merge to the published release.
2. **Fresh clean clone** for the release; never the owner's checkout.
3. **Rehearse the real release tree before GO:** in a throwaway worktree, apply
   exactly what the release commit will change (fold the changelog fragments,
   bump the version, refresh the lock), then run the full gate set on that
   tree, `scripts/ci-evidence` read-only against the candidate's own runs, and
   the `scripts/release --dry-run` with a no-writes check. A dry run alone does
   not see a folded tree or renamed CI jobs.
4. **Notes:** draft from the changelog fragments, keep a draft marker the
   runner refuses until the last merge, state limits honestly (verified versus
   accepted by policy), and carry still-open known issues.
5. **GO** names the exact `main` SHA and the notes file. Run the nightly
   evidence in parallel with the main evidence; a wrapper reruns the release
   script only on the evidence timeout or a rate limit and stops on any other
   failure.
6. **A failed release workflow after the tag:** rerun the failed jobs once;
   a second identical failure is a real defect, so stop, diagnose (diff the
   passing rehearsal logs against the failing release logs, then probe the
   hypothesis on a runner), fix, and release a new patch version rather than
   moving a pushed tag.
7. **After publishing:** verify the release (not a draft, asset count, a
   checksum spot check, catalog versions), attach the notes, comment the
   released-in line on the completed issues, close the milestone, lift the
   freeze, and tell every agent.
8. **Dogfood the upgrade** on the owner's host when asked: run the update,
   read the preflight, explain every at-risk session before accepting a loss,
   then `pohunek doctor`, and file every friction point as an issue.

## Talking to the owner

- Lead with the state and the next ETA; use a small table for multi-step
  timelines; give the realistic range and what can move it.
- When something slips, say why in one line, what you changed, and the new ETA.
  Own coordination mistakes plainly and record the lesson.
- A decision request gives two or three options with a recommendation and the
  cost of each.
