---
kind: fixed
---
A local `pohunek assistant` now starts its session in a dedicated
`<data-dir>/assistant` directory, created at mode 0700 when missing, so a
Claude Code folder-trust answer persists per host instead of re-asking on
every launch. An existing path that is not a directory the invoking user
owns, or that is group- or world-writable, is refused with
`assistant_workdir_unusable` before any daemon is started or dialed; the
launcher never chmods or replaces it. Pohunek never writes `~/.claude.json`.
A `session.new --input` whose grace expires while a hook-handshake runtime
(Claude) waits on a dialog now keeps the session instead of rolling it
back: the daemon delivers the prompt exactly once when the agent is ready,
`--json` carries `prompt_delivered: false` with `initial_input: pending`,
and `session.input` before the handshake is refused with
`session_input_blocked` (answer in `pohunek attach`).
A deferred prompt is discarded with a recorded reason when it can never
be delivered: `session_stopped` or `session_exited` on stop or exit,
`daemon_restart` on restart (the text is never persisted at rest and is
gone with the old daemon), and `delivery_failed` when the write itself
fails on a still-running session. `pohunek session show` prints the
pending or dropped state, and an empty `--input` keeps no deferred
prompt at all.
([#2406](https://github.com/zajca/pohunek/issues/2406))
