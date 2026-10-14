## Unreleased (2026-10-12, Pi dialogs, selectors and session-dir overrides, #547)

- The Pi package (version 1.0.2) reads Pi's dialogs as `blocked`: an
  extension's select, confirm, input or editor dialog (also a permission check
  raised mid-turn), the project trust prompt at startup, the missing session
  cwd prompt and every login step. A body that holds a selector list (a cursor
  row and a row indented by two spaces) withholds `idle`,
  so the model, session, tree, fork and settings selectors match no rule; a
  quoted draft (`> text`) stays `idle`. Pi's own confirms drawn like an
  extension dialog (project trust, missing session cwd, "Summarize branch?",
  "Import session"), the `/trust` selector and the login steps read `blocked`
  even when the owner opened them. `scripts/tests/test_pi_detect_regex.py`
  runs the generator in CI and requires `detect.toml` to hold its output. The
  1.0.1 manifest read 93 of 120 captured dialog screens and 36 of 50 selector
  screens as `idle`. Screens of a real Pi 1.0.2 at widths 20 to 200 are in
  `compat/pi/screens/overlays/`.
- A file existence check may declare `store_env` and `store_max_depth` (a
  variable that names the store itself, followed by the check),
  `relocating_args` and `relocating_settings` (store moves the check cannot
  follow). `session.new`, `session.resume` and `session.fork` refuse a launch
  that moves the store out of reach with the new code
  `runtime/agent_native_store_relocated` and a hint naming the variable. Pi
  declares `PI_CODING_AGENT_SESSION_DIR`, `--session-dir` and the `sessionDir`
  setting of the agent directory and the project. A settings path that is not
  a regular file (a FIFO, a directory) is refused without blocking. Recovery
  runs the same refusal, so a `sessionDir` written after launch, or the loss
  of the variable the launch had, refuses resume and fork; there the hint says
  to undo that change (remove the setting, or restore the variable to the path
  the session was launched with and accept the profile change). A crash of the
  check (on recovery the whole check, Claude's transcript check included) is
  `daemon/native_store_check_task_panicked`. The keys are frozen into the
  session's launch spec (store schema 7, a no-op migration from 6).
  Documented in the `pi-package` guide, `concepts/sessions`, the
  `debug-session-runtime` runbook, `docs/public-api.md`, `docs/install.md` and
  the source map.
