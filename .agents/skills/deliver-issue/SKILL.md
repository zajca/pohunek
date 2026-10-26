---
name: deliver-issue
description: >-
  Deliver one larger GitHub issue end to end without asking the operator
  anything: plan the stack, implement through parallel subagents, pass the
  gates, publish stacked PRs, then loop on CI and the Opus review marker of
  every pushed head — fixing or rebutting each finding — until the head is
  green with a passing marker, merge bottom-up, verify the landing, close the
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
  (accepted harness trade-offs (`.agents/instructions/workflow.md`)). It does not authorize
  releases, tags, force-pushes to `main`, or work outside the issue's scope.
- Never ask the operator. Resolve every ambiguity yourself: take the most
  conservative interpretation consistent with `docs/architecture.md`, the
  RFCs, and the issue's decisions, and record it on the issue as
  `Decision (autonomous): … because …`. The only stop conditions are: the
  issue does not exist or has no actionable scope, competing issues make the
  target unclear, or a required credential/permission is missing. Record a
  stop as a blocker comment on the issue, then report it.
- Never end a turn waiting for the operator. Waiting for CI or a review is
  done through whatever watch/monitor capability the runtime offers (a
  monitor tool, a background command, or a scheduled wakeup), so the next
  callback or notification resumes the work.
- File every verified out-of-scope finding as its own issue automatically
  (dedup, add to the project, link as a sub-issue of the delivered issue) per
  `github-workflow`. Never "offer" a follow-up.

## Phase 1 — intake

1. Resolve the issue per `github-workflow` (explicit number wins). Read the
   body **and all comments**; pin the DoD item IDs and the issue's
   `updatedAt`. Before each later phase re-read it and reconcile substantive
   spec drift explicitly (new/changed DoD, decisions) on the issue.
2. Set the project status to `In Progress`.
3. Create a local task list (any progress-tracking mechanism the runtime
   provides) mirroring the phases and DoD items; keep it
   current — it is the local progress view, the issue is the record.

## Phase 2 — plan

1. Read AGENTS.md, `docs/architecture.md`, and every doc the issue names.
   Before any `.rs` work, read `.agents/rust-guidelines/` per
   `.agents/instructions/coding.md` or the guidelines' own `SKILL.md` index.
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
   values, the testing policy (`.agents/instructions/verification.md#testing-policy`), knowledge-bundle updates), the local test
   environment rules (below), and the report format. Brief every worker with
   "read the context file first" plus its own 4-step briefing (known facts,
   `path:line` starts, testable success criteria, owned files).
2. Spawn the workers in parallel (several subagent/worker delegations in one
   step, using your runtime's parallel-worker mechanism). Each
   worker must:
   - **verify the task/finding against the code first** and report a false
     premise with `path:line` evidence instead of changing code;
   - for a bug, lifecycle, durability, concurrency, or security fix, add a
     regression scenario — an integration scenario driving cooperating
     production components through a supported boundary, or an E2E scenario
     driving the real product processes — extending the existing scenario
     when one fits, and show it fails without the change and passes with it;
     add no test for a trivial helper change or a refactor existing scenarios
     already cover, and never write a new unit test (testing policy (`.agents/instructions/verification.md#testing-policy`));
   - run the checks its change affects: its crate's tests, clippy `-D warnings` (host, and
     `scripts/darwin-check` for platform/daemon code), `cargo fmt`,
     and `cargo xtask docs check` when docs changed;
   - report root cause, changes with `path:line`, test evidence, and open
     points.
3. Follow-ups in a worker's area go to **the same worker** — resume or
   re-message the worker that owns that area (it keeps its context); where the
   runtime offers no way to resume a worker, spawn a fresh worker briefed
   with the previous one's report and owned files. Spawn fresh workers only
   for new areas.
4. Every "open point" a worker reports is resolved before the phase ends:
   fixed in this issue when it is in scope, otherwise filed as a follow-up
   issue. Nothing is left only in chat.
5. Review each worker's diff yourself before accepting it: scope, ownership,
   and comment rules (`git diff -U0 | grep` for history words such as
   "previously", "no longer", "now also", "used to", "as before").

## Phase 4 — gates, commit, publish

1. Run the full applicable gate set per the `gates` skill once on the final
   revision of each slice, on the whole worktree, not per crate. A gate that
   cannot run locally (see environment rules) is named as CI-only in the
   evidence, never silently skipped.
2. Commit per `pr-handoff`: explicit staging, `--no-gpg-sign`, concise
   imperative English message with the *why*, no trailers or footers.
3. Run a local Opus subagent review of `git diff origin/main...HEAD` for the
   slice and fix every finding at MEDIUM and above before the push; repeat it
   after every fix round. It is a pre-push quality check, not the review gate
   of Phase 5.
4. Push and open the PR stack sequentially per `pr-handoff` (evidence-built
   descriptions, `Refs #N` / final `Closes #N`). Keep higher slices rebased
   and gate-checked locally, and push only the lowest slice currently
   awaiting CI or review. Publish the next one after the prior head finishes;
   record unpublished slices as pending on the issue.

## Phase 5 — the CI + review loop

Every push creates a new head. The review gate is an `opus-pr-review:v1`
marker for that exact head, next to green CI. **Both** must be checked for
every head; checking only CI loses review rounds.

The marker is a comment on the PR conversation, authored by `zajca`, posted by
an Opus (high effort) review session that the maintainer's manager starts for
the pushed head. It carries

```text
<!-- opus-pr-review:v1:<repo>#<N>@<40-char head sha>:high=<h>:medium=<m>:low=<l> -->
```

followed by the findings. The rules:

- The comment author is `zajca`. A marker from any other login is not a
  review. No label starts or enables the review.
- The sha is the PR's current head. A marker for an earlier head does not
  carry over to a new head; when several markers name the head, the latest
  counts.
- GO is `high=0` and `medium=0`. A review with only LOW findings passes; each
  LOW finding is verified and filed as a follow-up issue per `github-workflow`.
- The delivering agent never posts, edits, or imitates a marker.

1. **Request the review.** The review session is started by the manager, not
   by the delivering agent. Report every pushed head (PR number, sha, CI
   state) to the manager through the channel the invocation names. When no
   review has arrived, say so again and record the wait on the issue; never
   push or amend a head only to provoke a review.
2. **Watch.** Start one background watcher per push (a monitor tool where the
   runtime has one; otherwise a polling loop or repeated scheduled checks)
   that polls until (a) every check of the PR is non-pending and (b) a marker
   for the pushed head exists:

   ```bash
   gh api "repos/<repo>/issues/<n>/comments" --paginate \
     --jq '.[] | select(.user.login == "zajca") | select(.body | contains("opus-pr-review:v1:<repo>#<n>@<sha>:")) | .html_url'
   ```

   Read `high`, `medium`, and `low` from the marker and the findings from the
   same comment; emit failed checks and the marker link as events. Also read
   human reviews, inline review comments, and PR conversation comments since
   the last round. Re-arm on expiry. Never merge without the marker of the
   final head.
3. **CI failure triage** — before changing anything:
   - Fetch logs only after the whole run completed (`gh run view <run> --job
     <job> --log-failed`; earlier it returns nothing).
   - Classify with evidence: regression from this diff; product race or
     ordering bug exposed by CI load; non-hermetic test (depends on the
     host's processes, paths, timing); pre-existing flake (check the test's
     history on this branch and `main` with `gh run list`). Reproduce locally
     under CI-like conditions when it is timing-related (build first with
     `cargo nextest run ... --no-run`, then wrap only the run in
     `scripts/stress-run --deadline <D> -- cargo nextest run ... --stress-count
     N`, never a detached load or one that uses every host CPU;
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
5. **Close the round.** Re-run the checks whose inputs the round's fixes
   changed (`cargo ta --print` names the affected crates; workspace-wide
   inputs such as `Cargo.toml`, `Cargo.lock`, `.config/nextest.toml`, or the
   CI workflow mean the full applicable gate set); checks with unchanged
   inputs keep their evidence. Before replacing a head whose PR run is still
   active, identify and cancel that PR's obsolete run per `pr-handoff`, then
   verify its final status. Commit and push to the owning slice branch;
   restack upper slices locally with `git rebase --update-refs`, and publish
   them bottom-up after the lower head's CI and review. Post one issue comment
   per round: marker link, each finding → fixed (`path:line`, test) / rejected
   (evidence) / follow-up (#issue), CI triage results, gate results, new
   head SHA. Go back to step 1 (report the new head).

**Merge criteria** for a PR (all must hold on its current head):
every required check green; the `opus-pr-review:v1` marker authored by `zajca`
names this exact head with `high=0` and `medium=0`, and every LOW finding is
filed as a follow-up issue; no unresolved human
review request; every DoD item the slice owns is met with evidence. A diff
that touches release workflows or xtask release code, runtime packages or
packaging, the TypeScript SDK, Cargo dependencies, or the protocol version
window also needs one green `tier=main` dispatch at the final head, started
after PR CI and the marker are green (`gh workflow run ci.yml --ref <branch>
-f tier=main`).

If the operator asked for **merge on green CI** in the invocation, merge as
soon as the checks are green, keep watching for the head's marker, and
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
- **Tests** follow the testing policy (`.agents/instructions/verification.md#testing-policy`): integration scenarios driving
  cooperating production components through a supported boundary, or E2E
  scenarios driving the real product processes, with independent expected
  results; no new unit tests, no coverage targets, no duplicates of covered
  behavior.
- Tests are deterministic (hooks, barriers, paused time — no sleeps) and
  hermetic (no dependence on the host's process table, paths, or timing).

## Local environment rules

- Run tests through `bash`, with every `POHUNEK_*` variable unset — zsh does
  not word-split `$VAR`, so a zsh `env $U …` silently removes nothing:
  `bash -c 'for v in $(compgen -e | grep "^POHUNEK_"); do unset "$v"; done; cargo nextest run …'`.
  Never print variable values. Never call `env` or `printenv` in any form
  (including `env | grep -c` to verify): a runtime permission or ask rule may
  fire on any occurrence and block the autonomous run on a confirmation;
  verify with `compgen -e | grep -c '^POHUNEK_'`.
- The relay PostgreSQL tests need `POHUNEK_RELAY_TEST_DATABASE_URL`; without
  it about 140 relay tests fail locally by design — they are CI-only
  evidence, not regressions.
- `cargo clippy --target aarch64-apple-darwin` cannot build `ring` or `psm` on a
  Linux host without an Apple SDK; run `scripts/darwin-check` instead (it needs
  `zig` on PATH). It only lints, so macOS behavior is CI-only evidence.
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
