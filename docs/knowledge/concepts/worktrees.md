---
type: Concept
id: concept/worktrees
title: Worktrees
description: Pohunek can bind sessions to repository worktrees so agent work is isolated from the main checkout.
source_kind: manual
intents: [project, debug, help]
---

# Worktrees

Pohunek sessions can run in place or in a dedicated Git worktree. A worktree is
requested through `pohunek session new` with a repository or project plus a
branch. The base branch is taken from `--base-branch`, then the project's
configured default, then the repository default. A requested base branch (the
flag or the project's configured default) is never replaced by another branch.
When it is not present locally, Pohunek fetches it from `origin`; if it does not
exist, or the fetch fails, the create fails with `invalid_base_branch` or
`base_branch_fetch_failed` and no worktree is made. A base that exists locally
is used even when its fetch from `origin` fails; the `session.new` result then
carries a `fetch` warning. Only a request without any base starts from the
repository's current branch.

The base branch only applies to a branch the worktree creates. A `--branch`
that already exists locally is checked out as it is. When `--base-branch` was
given explicitly and that existing branch is at another commit than the base,
for example a local pull-request branch behind its `origin` copy, the
`session.new` result carries a `base_branch_fallback` warning whose `detail`
names both commits. The warning arrives with the created session, so a caller
that must not let the agent act on another commit starts the session without
`--input`, checks `warnings`, and only then sends the prompt with
`session input`.

A branch the worktree creates from a base has no upstream. The base is fetched
into a temporary ref, not a remote-tracking branch, and an upstream set to a
base with another name would make a plain `git push` target the base branch.
Set one when the branch is first pushed, with `git push -u origin <branch>`.

Worktree operations on one repository wait for each other. Adding, removing and
pruning worktrees run one at a time, and a base-branch fetch waits for a running
add or prune and holds those back while it runs (fetches run side by side).
Each wait is bounded by the git command timeout (30 seconds), and the git
command that follows has its own 30 seconds, so one operation can take up to
about 60 seconds (a fetch also runs a few unlocked git steps, each with its own
30-second bound). A wait that runs out is reported like the operation's own failure, with the text "waiting for
another worktree operation on the same repository": as the `fetch` warning when
the base exists locally, as `base_branch_fetch_failed` when it does not, and as
a failed create or removal for an add or remove. The ordering holds within one
daemon; another daemon, or a manual `git worktree add` on the same repository,
is not ordered with it.

Worktrees are useful when an agent should make branch-scoped edits without
touching the main checkout. They also make remote project sessions practical:
the daemon resolves the project on the target host and creates the worktree in
that host's filesystem.

`pohunek project show <project> --json` lists the project's worktrees. A
worktree Pohunek created carries `owned: true` and `owner_session_id`, the
session it was created for, which stays after that session stops; `session_id`
appears only while a live session runs in the worktree. The human output flags
them as `owner=<id>` and `session=<id>`.

Removing a project record never deletes the main repository. The project remove
command has a separate `--prune-worktrees` option for worktrees Pohunek created;
it does not remove unrelated worktrees.

A pohunek-owned worktree is removed with `git worktree remove --force`, which
deletes the checkout but leaves the branch and its commits in the repository.
`pohunek session rm` does that unconditionally, since it is an explicit operator
action. The automatic session retention sweep does not: it keeps any session
whose owned worktree has uncommitted or untracked changes, commits contained in
no other ref, or a state git cannot report (see the Sessions concept's Retention
section). Cleanup is best-effort, so a checkout whose removal failed is reported
rather than silently counted as cleaned, and the leftover directory needs manual
cleanup.

`pohunek session new` writes a durable create record before it binds a
worktree, and the worktree binding before the checkout, so every worktree a
create makes can be traced back to its session. When the create fails after
the worktree was bound, the daemon removes that worktree only once the
session's worker is proven ended, even if the client disconnected meanwhile,
so the branch is free for a retry: it retires the worker, then removes the
checkout and its binding, then deletes the create record. If the worker
supervisor cannot confirm the worker ended, the create fails with
`runtime_supervision_unavailable`, the session stays listed as reconnecting,
and its worktree is kept until reconciliation proves the worker ended. A
checkout that cannot be removed (for example a worktree locked with `git
worktree lock`) keeps its binding and the create record: the create still
fails with its original error, the session stays listed as reconnecting with
reason `create_compensation_pending`, and the running daemon retries the
removal with the supervision backoff (1 s doubling to 60 s) until it succeeds,
so unlocking the worktree is enough; the next daemon start retries it too. A daemon that stops or crashes mid-create leaves
the same record, so the next start removes the half-created worktree; a
graceful stop first lets in-flight creates finish, including delivery of a
`--input` prompt, and refuses new ones with `daemon_shutting_down`. A create
whose prompt was not delivered before the daemon stopped is removed at the next
start, worktree included, since the prompt was never stored.

A session never starts outside its working directory. Before the worker spawns
the agent, it checks that the launch cwd is still a directory; when the worktree
was removed after binding, the launch is refused with `session_cwd_missing`,
nothing is started, and the failed create is compensated as above. The daemon
logs the bind, each removal with its reason, and a failed or timed-out
post-create or pre-remove hook at WARN, without the hook's output. Every hook
run also logs `hook started` and `hook finished` at INFO with the session id,
the event, the layer (`host` or `repository`), the script, and its timeout,
duration (`duration_ms`) and outcome. `session.new` answers only after its
`pre-create` and `post-create` hooks finished, each layer bounded by 5 minutes,
so these lines show whether a slow create was waiting on a hook.

A failing `pre-create` or `post-create` hook leaves a `hook` warning on the
session. In the `session.new` result its `detail` also carries the last
`hooks.stderr_tail_bytes` bytes of the hook's stderr (`service.toml`, default
4096, `0` disables), sanitized and marked when cut, so the caller can see why
setup failed. The stored session and the logs keep the warning without that
tail. Stdout is never kept, and a successful hook's stderr is discarded.
`session.new` never waits for processes a hook leaves running in the
background; their stderr is drained and discarded. When such a process still
holds the stderr two seconds after the hook ended, the daemon hands the pipe to
a small drain process (`/bin/cat` reading it into `/dev/null`, in a process
group of its own) and logs that at DEBUG with the hook's event and script and
the drain's PID. The drain outlives a daemon restart as the process does, so its
stderr writes keep succeeding, and it ends when the process closes its stderr.
A restart while the hook runs, or within those two seconds, still leaves the
process with a closed pipe: its next stderr write raises `SIGPIPE`. If the
drain cannot start, the daemon keeps discarding and logs a WARN instead. A hook
that starts long-lived processes can still redirect their stderr
(`>/dev/null 2>&1` or a log file); `hooks.stderr_tail_bytes = 0` keeps hook
stderr on `/dev/null`.

A host or repository `pre-remove` hook runs inside the worktree before
`pohunek session rm` deletes it, and `post-remove` runs in the repository
afterwards, so a project can release what its `post-create` hook provisioned
for that worktree (containers, networks, ports). The same pair also runs when a
failed create is rolled back, right after `post-create`, so both hooks must be
idempotent. A failing or timed-out remove hook never blocks the removal: the
`session rm` result carries a `hook` warning that names the event, printed by
the CLI and present in `--json` as `warnings`, since the session record no
longer exists to hold it.

Removing a worktree that a live session still uses is refused with the typed
`worktree_in_use` error; the same code is reserved for `session.remove` when
other active delegated tasks use the session's worktree. Stop the sessions or
tasks that use the worktree, then retry the removal.

Assistant guidance should preserve this boundary: verify which checkout or
worktree is active before editing, avoid deleting user-managed worktrees, and
prefer explicit project or repository targeting for project work.
