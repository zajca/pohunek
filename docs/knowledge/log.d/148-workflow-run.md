## Unreleased (2026-10-13, workflow run, owner knobs and run log, #148 #325)

- New `pohunek workflow run <plugin> <action> [-- <args>...]` runs one
  declared action of an installed, enabled plugin in text mode, after
  re-verifying its files. It runs without a shell, in the plugin's `files/`
  directory, with stdin from `/dev/null`, in its own process group, and with
  an environment of exactly `POHUNEK_PLUGIN_ID`, `POHUNEK_PLUGIN_VERSION`,
  `POHUNEK_PLUGIN_DIGEST`, `POHUNEK_PLUGIN_CONFIG_DIR`,
  `POHUNEK_PLUGIN_STATE_DIR`, `POHUNEK_PLUGIN_CONTEXT`, `POHUNEK_CLI`, the set
  `HOME`/XDG roots, and the
  `POHUNEK_SESSION_ID`/`POHUNEK_DAEMON_ID` pair when both are set, so
  `plugin_self_target_denied` still guards the hosting session.
- `POHUNEK_PLUGIN_CONTEXT` is a compact schema-1 JSON object with the plugin
  identity, the invocation (`kind`, `name`, ULID `run_id`), the output mode and
  the CLI version and protocol range.
- Deadlines, per-stream output caps and the `SIGTERM`-grace-`SIGKILL` stop come
  from the new owner knobs file `<config>/pohunek/workflow.toml` (`[limits]`,
  `[logs]`; strict, owner-only, defaults when absent). The CLI also stops the
  action's group when it receives `SIGINT`, `SIGTERM` or `SIGHUP`. Exit
  statuses: the action's own, 124 at the deadline, 125 over a cap, 128 plus
  the signal number when interrupted.
- Every run of an installed plugin, refused or not, appends a content-free
  record to `<state>/pohunek/workflow-plugin-state/<id>/logs/runs.jsonl`,
  except runs that fail before the run log is opened (for example invalid
  owner knobs, an unknown plugin, or a store or path failure).
- The action's output is delivered up to its cap whatever the speed of the
  CLI's own stdout and stderr consumer, unless the CLI is signalled, which
  ends the run as `interrupted`. An output pipe that an escaped descendant
  holds open is cut after one second not spent waiting for that consumer,
  whether the descendant writes or not, with a stderr warning.
- New error codes `workflow_config_invalid`, `workflow_plugin_disabled`,
  `workflow_action_unknown`, `workflow_action_args_refused`,
  `workflow_origin_incomplete`, `workflow_timeout_refused`,
  `workflow_context_too_large`, `workflow_run_log_failed`,
  `workflow_spawn_failed`, `workflow_run_failed`, `workflow_action_timeout`,
  `workflow_output_limit` and `workflow_action_interrupted`. No protocol
  method, event or SDK type was added; `--json` for `workflow run` is not yet
  available.
- Documented in the `workflow-plugins` guide, `safety/trust-model`, the source
  map, `docs/cli.md` and `docs/public-api.md`.
