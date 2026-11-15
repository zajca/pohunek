## Unreleased (2026-11-15, skip the in-repo hook layer, #2366)

- `session.new` takes `skip_repo_hooks` (behind
  `session_new_skip_repo_hooks_supported`; the Rust client and the TypeScript
  SDK refuse with `session_new_skip_repo_hooks_unsupported` and send nothing).
  The session then runs none of the in-repo hook layer (`.pohunek/hooks/<event>`
  and `.pohunek/setup`) for any of its seven lifecycle events, also after a
  resume, a daemon restart and in a create's rollback; the host-global layer
  still runs. `pohunek session new --no-repo-hooks` sends it. See
  [Worktrees](concepts/worktrees.md).
- Sessions record the switch (`SessionInfo.skip_repo_hooks`, on the resume
  binding and the worktree binding, store schema 21), and a keyed create
  fingerprints it.
- With the switch, a `hooks.required` event the host has no script for fails
  the create with `hook_required_skipped` before anything runs.
- `host.inspect` reports `host_hooks`, one boolean per event for the presence of
  a host-global hook script, also with `skip_runtime_probes`.
