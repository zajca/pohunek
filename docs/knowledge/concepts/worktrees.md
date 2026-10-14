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
configured default, then the repository default. When an explicit base branch
is not present locally, Pohunek fetches that branch from `origin` before it
considers falling back to the repository default.

Worktrees are useful when an agent should make branch-scoped edits without
touching the main checkout. They also make remote project sessions practical:
the daemon resolves the project on the target host and creates the worktree in
that host's filesystem.

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
post-create or pre-remove hook at WARN, without the hook's output.

A failing `pre-create` or `post-create` hook leaves a `hook` warning on the
session. In the `session.new` result its `detail` also carries the last
`hooks.stderr_tail_bytes` bytes of the hook's stderr (`service.toml`, default
4096, `0` disables), sanitized and marked when cut, so the caller can see why
setup failed. The stored session and the logs keep the warning without that
tail. Stdout is never kept, and a successful hook's stderr is discarded.
`session.new` never waits for processes a hook leaves running in the
background; their stderr is drained and discarded while the daemon runs, but
after a daemon restart their next stderr write raises `SIGPIPE`. A hook that
starts long-lived processes redirects their stderr (`>/dev/null 2>&1` or a log
file); `hooks.stderr_tail_bytes = 0` keeps hook stderr on `/dev/null`. When such
a process still holds the stderr two seconds after the hook ended, the daemon
logs a WARN naming the hook's event and script ("a background process still
holds the hook's stderr; ...").

Removing a worktree that a live session still uses is refused with the typed
`worktree_in_use` error; the same code is reserved for `session.remove` when
other active delegated tasks use the session's worktree. Stop the sessions or
tasks that use the worktree, then retry the removal.

Assistant guidance should preserve this boundary: verify which checkout or
worktree is active before editing, avoid deleting user-managed worktrees, and
prefer explicit project or repository targeting for project work.
