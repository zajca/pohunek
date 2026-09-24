---
name: github-workflow
description: >-
  Shared pohunek rules for recording and tracking agent work in GitHub Issues
  and the Pohunek Project: open or reuse an issue BEFORE starting meaningful
  work (auto-create after dedup), structure issue bodies vs comments, file
  verified follow-ups, set Project status honestly including Done-closure
  rules, persist safely with typed gh commands, and hand off work. Use
  whenever an agent starts meaningful work, creates, updates, or comments on
  an issue, adds anything to a project, resolves which issue names a task, or
  resumes interrupted work.
---

# github-workflow — GitHub Issues + Project tracking

GitHub Issues are the canonical record for pohunek work: scope, design
proposals and decisions, acceptance criteria, plans, verification evidence,
blockers, and handoffs. The Pohunek Project (owner `zajca`, repository
`zajca/pohunek`) tracks delivery status. Local files are not competitors:
`NEXT.md` is never required — a pre-existing untracked one is a read-only
migration source at most — and SiYuan notes are optional pointers.

## One config, no hardcoded IDs

Read `.github/agent-workflow.json` once per session before the first write. It
names the repository, the default project, the valid status options
(`Todo`, `In Progress`, `Done`) and their meaning, and how issue bodies vs
comments are used. Do not hardcode project numbers, field IDs, or status IDs
anywhere else; resolve the Project's field/status option IDs at run time with
`gh project view`/`gh project field-list`. There is no Review or Blocked
status: represent review findings and blockers as issue comments while status
stays `In Progress`. Being tracked as `Todo` for a design-proposal issue does
not imply the proposed design is accepted — acceptance is an explicit
decision recorded on the issue.

## Gate meaningful work on an issue first

No meaningful work starts without something on the triage record. Before
starting it: resolve the issue (below), deduplicate against existing issues
(paginated, open + recently closed), and then:

- No matching issue and the user gave a concrete new scope → **auto-create**
  the issue and add it to the configured project without a further ask. Do
  not require the user to say "file an issue".
- Multiple plausible matching issues (competing candidates) or a genuinely
  ambiguous scope → ask which issue/scope applies; do not pick a probable one.
- One unique match → reuse it (update scope/DoD, add a progress comment).

Explicit issue URL/number from the task or user always wins once verified to
exist and be open.

## Create or update — deduplicate first

Search for a matching actionable issue before creating one (open and recently
closed, paginated: `gh search issues` / `gh issue list --search`, following
pages until results stop growing). If one matches, update it instead of
opening a near-duplicate; link a genuinely distinct older issue when related.
Create a new issue only for meaningful, actionable work — a design proposal
with a decision, a milestone/feature/fix with scope and acceptance criteria,
or a verified follow-up discovered during work. Never file an issue per
trivial tool call, quick task, or in-flight detail; those belong in comments
on the issue already covering the work.

Automated follow-up issues: when work produces a verified out-of-scope
finding (a real gap discovered and evidenced outside the current DoD), file a
follow-up issue for it automatically (dedup first). Never move an unmet
original DoD item into a follow-up to claim the current issue done — the
issue stays open at `In Progress` until its own DoD is met.

## Issue body vs comments

- **Body** (long-lived, structured): full scope; accepted decisions with
  rationale; definition-of-done items with stable IDs (`D1`, `D2`, ...);
  dependencies (linked issues/APIs, including parent/sub-issue links); the
  plan. Preserve user-written sections when editing — edit or extend your own
  sections, read the body first.
- **Comments** (append-only): progress, verification evidence (exact commands
  and their results), handoffs between agents, blockers, and the reason for
  any status change. Never overwrite or delete another author's comment.

## Project membership and status

Actionable tracked work joins the configured default project and stays there
from "work accepted" to "verified completion". After any update, verify from
the API response (or a follow-up read) that the item actually exists in the
project with the intended status. Existing topic projects stay available as
additional views; do not remove items from them and do not migrate anything
destructively. Only sync a topic project when the task explicitly covers it.

### Done, issue closure, and status consistency

- Any **change to the repository** — code, scripts, harness files,
  documentation, skills, or configuration — reaches `Done` plus issue closure
  only via the same bar: *verified landing on the remote default branch* with
  *every DoD item met with evidence*. Publishing a change someone cannot
  verify on the remote default branch is not a landing, whatever kind of file
  it touched.
- A **local merge without an authorized push is not a landing**: the issue
  stays open at `In Progress` with a comment recording the local merge commit
  as evidence. The push it needs is still governed by the standing "commit/
  push only when asked" rule; no local merge widens that authorization.
- Only **pure planning/investigation with no repository change** (research,
  design decisions, issue-held plans) can complete on issue-held artifact and
  evidence alone — the artifact IS the issue — before any landing.
- Never close an issue whose DoD is partially met, and never set `Done`
  merely because something was written locally or a PR was opened.

## Safe persistence

- Read before you write; never blind-overwrite a body or project field.
- Verify every mutation from its API response or a follow-up read (`gh issue
  view`, the GraphQL item object) before claiming it landed.
- Search with pagination; do not treat the first page as the whole inventory.
- After an ambiguous mutation (timeout, ambiguous error), stop: read back the
  current state before any retry, and never retry blindly. When verifying
  project membership after a write, do not rely on `gh project item-list` —
  it can be stale; prefer the mutation's returned project item ID: a direct
  GraphQL `node(id: $projectV2ItemId)` lookup (reading `project`, `content`,
  and `fieldValueByName`), or the issue's `projectItems`. If targeted reads
  still disagree, report synchronization as unverified and do not repeat
  the mutation until its outcome is known.
- A permission, authentication, or network error **blocks sync**: report which
  GitHub write could not happen and stop claiming progress; do not falsely
  say "recorded" and do not work around permissions by other means.
- Writes you may make within an owner-authorized task: issues on the
  configured repository, comments, and project add/status updates relevant to
  that task. Never widen authorization (no unrelated auth changes, no team /
  relay grants) and never perform destructive project edits (deleting fields,
  statuses, or views, removing items).

## Usable safe gh command patterns

These are the shapes to start from; check `gh <cmd> --help` for exact flags
before a first use. Multiline text always goes through a file:

```bash
# read before write, with the repository resolved from the config
gh issue view "$issue_number" --repo "$repository" --json number,title,state,body,comments,projectItems
gh issue create --repo "$repository" --title "$issue_title" --body-file body.md
gh issue edit "$issue_number" --repo "$repository" --body-file body.md   # full replacement, read first
gh issue comment "$issue_number" --repo "$repository" --body-file note.md
# Preserve native parent/sub-issue and blocking relationships, plus readable links.
#   parent: a sub-issue via the documented REST sub-issues endpoint
#   (POST /repos/{owner}/{repo}/issues/{issue_number}/sub_issues) or GraphQL
#   addSubIssue, against the parent issue. Use the supported issue-dependency
#   API for blocking relationships and include "Depends on #N" in the body.
#   If a relationship write is unavailable, record it as pending; a text link
#   alone is not proof that the native relationship exists. Confirm any `gh`
#   wrapper's local --help before preferring it over the endpoints above.
```

For project-item readback, bind the returned item ID and the status field name
discovered from the project's fields:

```bash
gh api graphql -f projectV2ItemId="$project_item_id" -f statusField="$status_field_name" -f query='
query($projectV2ItemId: ID!, $statusField: String!) {
  node(id: $projectV2ItemId) {
    ... on ProjectV2Item {
      id
      project { number title }
      content { ... on Issue { number repository { nameWithOwner } state } }
      fieldValueByName(name: $statusField) {
        ... on ProjectV2ItemFieldSingleSelectValue { name }
      }
    }
  }
}'
```

Issue numbers here are placeholders (`$issue_number`, `$repository`): none
of these examples is meant to run against a live issue without filling them
in from the resolved task.

Body rewrites: read the current body, apply your edits to your own sections,
write the full new body to a scratch file, `gh issue edit N --body-file`, then
read back and diff to confirm the intended sections survived; remove the
scratch file.

## Handoff comments (stopping or leaving the work)

When a task ends — completed, blocked, or handed to another agent/session —
leave one handoff comment on the issue with, at minimum:

- the branch and worktree paths the work lives in;
- the HEAD revision (or, for pure planning/investigation, the issue-held
  artifact itself);
- what scope was covered and what explicitly remains;
- the exact checks run with their real exit results, including any skipped or
  failed checks and why;
- remaining work / open blockers;
- for runs that spawn more agents: the worker run IDs.

A handoff is the resume point: the next read of the issue must be enough to
continue without local-only memory.

## Runtime discipline

Read the issue's current body and comments before resuming any interrupted
work; rely on the issue, not memory, for state. When a decision revises an
accepted design constraint in `docs/architecture.md` or `docs/design/`, the
issue must state that revision explicitly — accepted docs stay authoritative
until deliberately revised.
