# Workflow

The issue-driven milestone harness starts with
`scripts/harness-milestone <slug> --issue NUMBER|URL`. It resolves the
repository and project from `.github/agent-workflow.json` and validates the
issue before changing Git state. Its executable checks run with
`python3 -m unittest discover -s scripts/tests -p 'test_*.py'`.

- Work on a branch off `main`; do not commit directly to `main`. Commit/push
  only when the user asks.
- **Work is tracked in GitHub Issues and the Pohunek Project**, not in local
  planning files. GitHub Issues are the canonical record for scope, design
  proposals and decisions, acceptance criteria (DoD items with stable IDs —
  `D1`, `D2`, ...), plans, verification evidence, blockers, and handoffs; the
  Pohunek Project tracks delivery status (`Todo` / `In Progress` / `Done`).
  `NEXT.md` is no longer an authority and no workflow requires it;
  `docs/design/` stays for accepted long-lived technical design. The shared
  rules — issue resolution, deduplication, body/comment structure, project
  status semantics, and safe persistence — live in the **`github-workflow`
  skill** (`.agents/skills/github-workflow/`) with configuration in
  `.github/agent-workflow.json`; every agent must follow it for any GitHub
  issue/comment/project write.
- **Gate meaningful work on an issue first.** Before meaningful work starts,
  resolve its issue: use an explicit issue URL/number when given, otherwise
  deduplicate against existing issues. When the user gives a concrete new
  scope and no matching issue exists, **auto-create** the issue and add it to
  the configured project without a further ask. Issue/comment/project writes
  on the configured repository are a standing owner authorization: never ask
  whether to file an issue, and file verified defects noticed incidentally
  too (see the `github-workflow` skill). Ask only when the scope is
  genuinely ambiguous or competing issues both plausibly cover it. Verified
  out-of-scope findings get their own follow-up issue automatically, but an
  unmet original DoD item never moves to a follow-up to claim the issue done.
- **The milestone loop runs against a GitHub issue.** Development moves one
  milestone at a time. The loop is: the phase plan and its DoD land as a
  GitHub issue (`plan-phase`) → implement it in a fresh worktree off `main`
  (`milestone`) → review the branch against the issue's DoD
  (`milestone-review`) → land it locally via `merge-advance` or publish it
  via `pr-handoff`, with the issue and project updated accordingly (issue
  closure + `Done` only after verified landing on the remote default branch
  for any repository change; only pure planning/investigation with no
  repository change completes on the issue-held artifact). Do not create new
  local `NEXT.md`/RFC files as work-management authorities; a pre-existing
  untracked `NEXT.md` is a read-only migration source at most.
- **Plans are end-to-end complete.** Do not propose or build PoCs, minimal
  versions, or phased-minimal shortcuts unless the user explicitly asks for
  reduced scope. Plan and implement the full solution.
- **Ship a stack of small sequential PRs, not one large PR.** Split the work
  into ordered slices, one coherent concern each, stacked branch on branch and
  landed bottom-up. Each PR is complete for its slice — builds, passes the
  applicable full gate set alone, carries the behavioral scenarios its change
  needs under the [testing policy](verification.md#testing-policy), updates `docs/knowledge/` for surfaces it
  touches — with no stubs a later PR fills in; the stack as a whole delivers
  the full DoD, so slicing is not a PoC shortcut. The slice plan is recorded on
  the issue first. Size cue and mechanics: `pullRequests` in
  `.github/agent-workflow.json` and the `pr-handoff` skill.
- **Budget GitHub Actions capacity across PRs.** For independent PRs, coordinate
  head pushes so their CI runs do not start together when the shared runner
  capacity is saturated. For a stack, rebase and verify higher branches locally,
  but push only the lowest PR that currently needs CI or review. Wait for its
  current head to finish CI and review before publishing the next branch; do
  not batch-push a restacked train. If a new push supersedes an unfinished PR
  run, cancel that run or verify GitHub's same-ref concurrency already canceled
  it, and stop watching the obsolete head. Cancel only runs belonging to the
  PR being updated. Every published head still needs its own required checks
  and review before merge; see `pr-handoff` and `deliver-issue` for the steps.
- **Commits are never signed.** Use clean, concise, English messages. Do not add
  a `Co-Authored-By` trailer or any "generated with" footer.
- Keep changes scoped. If you touch the wire protocol (`crates/protocol`), expect
  ripples in `client`, `daemon`, and `cli`, and into `docs/public-api.md` and the
  `docs/knowledge/` bundle — update all of them and run their checks. A non-additive wire change
  (rename, moved field) bumps `PROTOCOL_VERSION`, adds the edge adapter for the
  version it leaves behind in `crates/protocol/src/compat/` with golden fixtures
  and consumer recordings produced by that release's own code, and deletes the
  oldest adapter in the same change. A semantic change (new required parameter,
  removed method or event, changed error code or meaning) cannot be adapted: it
  also raises `MIN_PROTOCOL_VERSION` and is announced as a break. A method added after the previous release is listed in the adapter's `INTRODUCED_METHODS` so older connections get `method_not_found`.
- Run the applicable full gate set once on the final revision before declaring
  done; iterate on affected checks in between (see the [testing policy](verification.md#testing-policy)). Report
  failures honestly with output; never claim green without running it.
- When a task spans 3+ steps, plan first and verify after each major step.
- Delegate milestone implementation and post-review fixes to parallel
  subagents or another coding agent by default; this is the standing mode of
  work here, not something to wait for permission on. Brief every worker with
  concrete `path:line` context, because it starts with a clean context. Good
  fits: a pre-PR review split into security (secret handling, the `sh -c`
  attach surface, the NetBird trust boundary), silent-failure (swallowed errors
  in the daemon and state machines), and performance passes, then synthesized;
  one implementer per affected crate for a protocol change (`protocol` →
  `client`/`daemon`/`cli`/`sdk/ts`, plus `docs/public-api.md` and
  `docs/knowledge/`); competing-hypothesis investigation for state-machine bugs
  across `client`/`daemon` session detection.

## Accepted harness trade-offs (operator decisions)

The milestone loop in `.lh-harness/workflows/` deliberately trades partial
security for autonomy. These are recorded operator decisions, not oversights;
do not re-report them as review findings:

- A harness run whose final audit reports every DoD item met with the full
  gate set green **pre-authorizes** the `pr-handoff` flow (commit, push,
  open PR) without a further ask. This intentionally supersedes the default
  "commit/push only when the user asks" for that one path; it is publishing
  authorization only and does not authorize merging.
- Invoking the `deliver-issue` skill on an issue is the owner's explicit
  request to commit, push, open that issue's PRs, and merge them once the
  skill's merge criteria hold (green checks and a review of the final head
  with no unanswered actionable finding). It covers that one issue only and
  never releases, tags, or force-pushes `main`.
- Harness executors run their model CLI unsandboxed on the operator's host;
  `--workspace` scopes the working directory, not file access.
