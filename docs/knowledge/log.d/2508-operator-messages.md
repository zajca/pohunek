## Unreleased (2026-11-26, operator messages, #2508 #2509 #2540)

- `session_cwd_missing` (runtime) carries the recovery hint `the working directory <abs path>
  does not exist; nothing was started`. When the directory is the assistant working
  directory (`<data_dir>/assistant`, shared through `pohunek_paths::ASSISTANT_WORKDIR_SUBDIR`),
  the hint adds that `pohunek assistant` creates it. The error code and JSON shape are unchanged.
- The `session_input_not_ready` message reads the activity once: a `blocked` agent is a kept
  create, so the "raise input.initial_startup_grace_ms" sentence never sits next to
  `last activity: blocked`.
- `pohunek session new` (human output) prints a `waiting on a dialog` line when the kept session
  is `blocked` and the daemon reports the initial input as not delivered, with or without
  `--input-ack-timeout-ms`; it names `pohunek session screen <id>` and `pohunek attach <id>`
  (`<host>/<id>` for a session on another host).
- Claude package 2.3.1: `claude-accounts add <name>` reports `<name>` in its success message, and
  `claude-accounts add` runs on macOS: its wrapper step passes `--` before the mode (`chmod -- 755 file`),
  because BSD `chmod` reads a `--` after the mode as a file name.
