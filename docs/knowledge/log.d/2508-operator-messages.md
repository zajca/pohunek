## Unreleased (2026-11-26, operator messages, #2508 #2509 #2540)

- `session_cwd_missing` (runtime) carries the recovery hint `the working directory <abs path>
  does not exist; nothing was started`. When the directory is the assistant working
  directory (`<data_dir>/assistant`, shared through `pohunek_paths::ASSISTANT_WORKDIR_SUBDIR`),
  the hint adds that `pohunek assistant` creates it. The error code and JSON shape are unchanged.
- The `session_input_not_ready` message reads the activity once: a `blocked` agent is a kept
  create, so the "raise input.initial_startup_grace_ms" sentence never sits next to
  `last activity: blocked`.
- `pohunek session new` (human output) prints a `waiting on a dialog` line after
  `prompt delivered: no` when the kept session is `blocked`; it names `pohunek session screen <id>`
  and `pohunek attach <id>`.
- Claude package 2.3.1: `claude-accounts add <name>` reports `<name>` in its success message.
