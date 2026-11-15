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
  fingerprints it. The schema step is the fail-closed choice: an older daemon
  refuses the schema-21 store instead of dropping the switch and running the
  checkout's hooks; a rollback restores the `metadata.jsonl.pre-schema-20`
  backup.
- With the switch, a `hooks.required` event the host has no script for fails
  the create with `hook_required_skipped` before anything runs.
- With the switch, the session runs none of git's own hooks and no fsmonitor
  hook: a relative `core.hooksPath` or `core.fsmonitor` would otherwise
  resolve to a program the checkout carries for a git command run inside it
  (the daemon's `git status` of `session diff` and of the retention
  pre-removal check ran them, and so did the agent's git), and `session diff`
  ran the textconv driver the checkout's `.gitattributes` selected, and the
  daemon's checkout, `git status` and `git diff` ran the filter drivers it
  selected. Every daemon git command of the session takes `-c
  core.hooksPath=/dev/null -c core.fsmonitor=false` (the hook layer is a
  required argument of the daemon's git command constructor) and turns off
  every filter driver its configuration defines, listed just before it runs
  and passed as `GIT_CONFIG_COUNT` pairs (a driver name may hold the `=` that
  `git -c` splits at); its `session diff` adds `--no-textconv`. The agent, on
  create and on every relaunch, and the host hook scripts get the hook and
  fsmonitor settings as two more `GIT_CONFIG_COUNT` pairs behind the existing
  ones; their own git still applies filter drivers. `GIT_CONFIG_PARAMETERS`
  or a non-numeric `GIT_CONFIG_COUNT` in the agent environment fails the
  start with `git_hooks_env_conflict`.
- The switch is per session; a child or a later in-place session in the same
  checkout does not inherit it.
- `host.inspect` reports `host_hooks`, one boolean per event for the presence of
  a host-global hook script, also with `skip_runtime_probes`.
