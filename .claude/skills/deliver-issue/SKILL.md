---
name: deliver-issue
description: >-
  Deliver one larger GitHub issue end to end without asking the operator
  anything: plan the stack, implement through parallel subagents, pass the
  gates, publish stacked PRs, then loop on CI and the automated PR review of
  every pushed head — fixing or rebutting each finding — until the head is
  green with a clean review, merge bottom-up, verify the landing, close the
  issue, and file every verified follow-up. Use when the operator hands over
  an issue to be finished autonomously ("dotáhni issue #N", "deliver #N",
  "work on #N until it's merged").
---

# deliver-issue — autonomous issue-to-merge delivery

The operator hands over one issue and expects it back **merged, verified, and
closed** — not a question, not a partial state. This skill chains the
existing loop skills (`github-workflow` → `milestone` → `gates` →
`pr-handoff` → `merge-advance`) and adds what they leave to the operator:
the review loop, CI triage, and the merge decision. Where a step below says
"per `<skill>`", follow that skill; this file only states what differs.

## Authorization and autonomy

- Invoking this skill **is** the owner's explicit request to commit, push,
  open the issue's PRs, and merge them once the merge criteria (below) hold
  (AGENTS.md, "Accepted harness trade-offs"). It does not authorize
  releases, tags, force-pushes to `main`, or work outside the issue's scope.
- Never ask the operator. Resolve every ambiguity yourself: take the most
  conservative interpretation consistent with `docs/architecture.md`, the
  RFCs, and the issue's decisions, and record it on the issue as
  `Decision (autonomous): … because …`. The only stop conditions are: the
  issue does not exist or has no actionable scope, competing issues make the
  target unclear, or a required credential/permission is missing. Record a
  stop as a blocker comment on the issue, then report it.
- Never end a turn waiting for the operator. Waiting for CI or a review is
  done with a Monitor or a background command, so the next notification
  resumes the work.
- File every verified out-of-scope finding as its own issue automatically
  (dedup, add to the project, link as a sub-issue of the delivered issue) per
  `github-workflow`. Never "offer" a follow-up.

## Phase 1 — intake

1. Resolve the issue per `github-workflow` (explicit number wins). Read the
   body **and all comments**; pin the DoD item IDs and the issue's
   `updatedAt`. Before each later phase re-read it and reconcile substantive
   spec drift explicitly (new/changed DoD, decisions) on the issue.
2. Set the project status to `In Progress`.
3. Create a task list (TaskCreate) mirroring the phases and DoD items; keep it
   current — it is the local progress view, the issue is the record.

## Phase 2 — plan

1. Read AGENTS.md, `docs/architecture.md`, and every doc the issue names.
   Before any `.rs` work, read `.agents/rust-guidelines/` per CLAUDE.md.
2. Split the work into a PR stack per `milestone` step 4 and
   `pullRequests` in `.github/agent-workflow.json`: one concern per slice,
   dependency order, each slice mapped to its DoD items. Record the slice
   plan in the issue body.
3. For each slice, build a **file-ownership matrix**: which worker owns which
   files. Two workers never own the same file; work on one file is
   sequenced, not parallel.

## Phase 3 — implement through subagents

Create the worktree per `milestone` step 2 with `scripts/worktree-new
<slug>`: it lands in `pohunek-worktrees/<slug>` beside the primary checkout
(the parent of `git rev-parse --path-format=absolute --git-common-dir`)
whichever checkout it runs from, and prints that absolute path (**never under
`/tmp`**, a small RAM tmpfs) with a reflink-seeded `target/`. Then, per
slice:

1. Write a **shared context file** (in the session scratchpad) holding: the
   worktree path and HEAD, "no commits, no pushes, no branches", the mandatory
   repo rules (Rust guidelines, comment rules below, no hardcoded tuning
   values, tests for every change, knowledge-bundle updates), the local test
   environment rules (below), and the report format. Brief every worker with
   "read the context file first" plus its own 4-step briefing (known facts,
   `path:line` starts, testable success criteria, owned files).
2. Spawn the workers in parallel (one message, several Agent calls). Each
   worker must:
   - **verify the task/finding against the code first** and report a false
     premise with `path:line` evidence instead of changing code;
   - add a regression test and show it fails without the change and passes
     with it;
   - run its crate's tests, clippy `-D warnings` (host, and
     `--target aarch64-apple-darwin` for platform/daemon code), `cargo fmt`,
     and `cargo xtask docs check` when docs changed;
   - report root cause, changes with `path:line`, test evidence, and open
     points.
3. Follow-ups in a worker's area go to **the same worker** via SendMessage
   (it keeps its context); spawn fresh workers only for new areas.
4. Every "open point" a worker reports is resolved before the phase ends:
   fixed in this issue when it is in scope, otherwise filed as a follow-up
   issue. Nothing is left only in chat.
5. Review each worker's diff yourself before accepting it: scope, ownership,
   and comment rules (`git diff -U0 | grep` for history words such as
   "previously", "no longer", "now also", "used to", "as before").

## Phase 4 — gates, commit, publish

1. Run the full gate set per the `gates` skill on the whole worktree, not
   per crate. A gate that cannot run locally (see environment rules) is
   named as CI-only in the evidence, never silently skipped.
2. Commit per `pr-handoff`: explicit staging, `--no-gpg-sign`, concise
   imperative English message with the *why*, no trailers or footers.
3. Push and open the PR stack per `pr-handoff` (evidence-built
   descriptions, `Refs #N` / final `Closes #N`) and label every PR
   `ai:review`, the only trigger of the automated review. Comment the stack
   links on the issue.

## Phase 5 — the CI + review loop

Every push creates a new head, and the automated review (a PR review whose
body starts with `<!-- hermes-codex-review:<repo>#<pr>:<sha> -->`) lands per
head commit, typically minutes after CI. **Both** must be checked for every
head; checking only CI loses review rounds.

1. **Label first.** The automated review runs only for a PR labeled
   `ai:review`. Before arming the watch verify the label with `gh pr view
   <n> --json labels` and add it when missing or removed (`gh pr edit <n>
   --add-label ai:review`; `gh label create ai:review` first if the label
   does not exist). Do this for every PR of the stack and after every push.
2. **Watch.** Start one Monitor per push that polls until (a) every check of
   the PR is non-pending and (b) a review whose `commit_id` equals the pushed
   head exists (`gh api repos/<repo>/pulls/<n>/reviews`); emit each failed
   check and the review id as events. Also read human reviews, inline review
   comments, and PR conversation comments since the last round. Re-arm on
   expiry. The reviewer normally answers within about 10 minutes of the push.
   If no review of the head exists 15 minutes after the push (or after CI
   finished, whichever is later), the reviewer is stuck: first check the
   `ai:review` label (a missing label is the usual cause), then re-fire the
   `labeled` event by removing and re-adding `ai:review` (no new head, so no
   CI churn), and only if that also gets no review within 15 minutes
   re-trigger it by pushing a new head — rebase onto the current `main` when it moved,
   otherwise `git commit --amend --no-edit --date=now` and
   `--force-with-lease` with the old head SHA — and record the retrigger on
   the issue. Repeat at most three times; after that record the blocker on
   the issue and keep waiting with long wakeups. Never merge without the
   review of the final head.
3. **CI failure triage** — before changing anything:
   - Fetch logs only after the whole run completed (`gh run view <run> --job
     <job> --log-failed`; earlier it returns nothing).
   - Classify with evidence: regression from this diff; product race or
     ordering bug exposed by CI load; non-hermetic test (depends on the
     host's processes, paths, timing); pre-existing flake (check the test's
     history on this branch and `main` with `gh run list`). Reproduce locally
     under CI-like conditions when it is timing-related (`stress --cpu N`,
     slow fsync via `strace -f -e inject=fsync,fdatasync:delay_enter=<µs>`,
     a helper spawning non-dumpable processes, a symlinked/long `TMPDIR`).
   - Fix the root cause. A test is aligned only to the product's real
     contract (e.g. the product hook's timeout), never weakened below it.
     A pre-existing flake outside the diff gets its own issue (with root
     cause); rerun the failed job only after that.
4. **Review findings** — for each finding of the round:
   - Verify it against the code. A false positive is answered, not coded
     around: record the evidence (`path:line`, spec/manual reference, a
     reproduction) in the round's issue comment.
   - Group real findings by file ownership and dispatch them to workers
     (Phase 3 rules; resume the worker that owns the area).
   - Prefer the principled fix the finding points at over a narrow patch,
     and scan the touched lifecycle for the same pattern (e.g. every durable
     write after releasing a lock) — fix all occurrences in the round.
   - A finding whose proper fix is a separate design decision outside the
     issue's DoD becomes a follow-up issue with the evidence; one inside
     the DoD is fixed here, never deferred.
5. **Close the round.** Gates (Phase 4.1), commit, push to the owning slice
   branch (restack upper slices with `git rebase --update-refs` and
   `--force-with-lease` per `pr-handoff`), then post one issue comment per
   round: review id, each finding → fixed (`path:line`, test) / rejected
   (evidence) / follow-up (#issue), CI triage results, gate results, new
   head SHA. Go back to step 1 (re-check the `ai:review` label).

**Merge criteria** for a PR (all must hold on its current head):
every required check green; the automated review of that exact head exists
and has no actionable finding, or each remaining finding was answered with
evidence as a false positive; no unresolved human review request; every
DoD item the slice owns is met with evidence.

If the operator asked for **merge on green CI** in the invocation, merge as
soon as the checks are green, keep watching for the head's review, and
address its findings in a new fix PR off the updated `main` (same loop).

## Phase 6 — merge, verify, close

1. Merge bottom-up per `merge-advance` / `pr-handoff` constraints
   (`gh pr merge <n> --merge`; retarget the next slice with `gh pr edit
   --base main`, rebase it when needed). Never merge an upper slice first.
2. Verify the landing: `git fetch` and confirm the merge commit is on
   `origin/main`; watch the `main` CI run for that commit to finish green
   (a red `main` is fixed immediately in a new PR through the same loop).
3. Close the issue per `github-workflow` (DoD verdict table with
   `path:line` evidence, merged PR links, gate results) and set `Done` only
   after the verified landing. Verify every write from the API response.
4. Clean up per `merge-advance` step 3 (worktree, local branches).
5. Final report to the operator: merged PRs, DoD verdicts, review rounds
   handled (count, rejected findings), follow-up issues filed, and anything
   CI-only that was not verified locally.

## Rules the workers inherit

- **Comments** state current behavior or reason, never history ("previously",
  "no longer", "as before", "this replaces …"); the before/after story goes
  in the commit message and PR description.
- **No hardcoded tuning values**: reuse existing config/settings, or add a
  config knob in the same pattern; document magic values
  (`M-DOCUMENTED-MAGIC`).
- **Durability and concurrency**: a destructive step runs only after the
  durable write it depends on applied (check the result, never log-and-go);
  a conditional write is compared against the record as persisted; never
  write a stale snapshot after releasing a lock; background paths (retries,
  watchers, reconciliation) must not act on work a live task still owns.
- **Fail closed**: uncertain evidence never deletes, kills, or retires
  anything; it keeps a visible, retryable state.
- Tests are deterministic (hooks, barriers, paused time — no sleeps) and
  hermetic (no dependence on the host's process table, paths, or timing).

## Local environment rules

- Run tests through `bash`, with every `POHUNEK_*` variable unset — zsh does
  not word-split `$VAR`, so a zsh `env $U …` silently removes nothing:
  `bash -c 'for v in $(compgen -e | grep "^POHUNEK_"); do unset "$v"; done; cargo nextest run …'`.
  Never print variable values. Never call `env` or `printenv` in any form
  (including `env | grep -c` to verify): the user's `Bash(env)` ask rule
  fires on any occurrence and blocks the autonomous run on a confirmation;
  verify with `compgen -e | grep -c '^POHUNEK_'`.
- The relay PostgreSQL tests need `POHUNEK_RELAY_TEST_DATABASE_URL`; without
  it about 140 relay tests fail locally by design — they are CI-only
  evidence, not regressions.
- `cargo clippy --target aarch64-apple-darwin -p pohunek-cli` cannot build
  `ring` on a Linux host; macOS behavior of the CLI is CI-only evidence.
- `/tmp` is a small tmpfs: no worktrees or target dirs there. macOS puts
  temporary directories below the `/var` symlink — canonicalize test roots
  that trusted-directory checks inspect.
- `dash` and `shellcheck` may be missing locally; shell scripts must stay
  POSIX (CI runs Ubuntu `dash` and macOS `/bin/sh`).
- Git in a worktree-isolated session: run plain `git`/`gh` commands from the
  worktree; never `git stash` without a unique tag.

## Constraints

- One issue per invocation; its DoD is the contract — no scope creep, no
  unmet DoD item moved to a follow-up to claim completion.
- No PoC, stubs, mocks of real implementations, or placeholders.
- Never sign commits; no `Co-Authored-By` or generated-by footer.
- Never weaken a test or a safety check to get green; never merge red.
- Every GitHub write follows `github-workflow` safe persistence; a blocked
  write is reported as blocked, never as done.
