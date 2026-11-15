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
- With the switch, the session runs none of git's own hooks: a relative
  `core.hooksPath` would otherwise resolve to hooks the checkout carries for a
  git command run inside it (the daemon's `git status` of `session diff` and
  of the retention pre-removal check ran them, and so did the agent's git).
  Every daemon git command of the session takes `-c core.hooksPath=/dev/null`
  (the hook layer is a required argument of the daemon's git command
  constructor); the agent, on create and on every relaunch, and the host hook
  scripts get the setting as one more `GIT_CONFIG_COUNT` pair behind the
  existing ones. `GIT_CONFIG_PARAMETERS` or a non-numeric `GIT_CONFIG_COUNT` in
  the agent environment fails the start with `git_hooks_env_conflict`.
- The switch is per session; a child or a later in-place session in the same
  checkout does not inherit it.
- `host.inspect` reports `host_hooks`, one boolean per event for the presence of
  a host-global hook script, also with `skip_runtime_probes`.
