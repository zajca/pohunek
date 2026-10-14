## Unreleased (2026-10-14, screen readiness for Pi and Hermes, #1094)

- The Pi and Hermes packages declare `[input] ready = "screen_idle"`. A
  `session.new` with initial input into either runtime (for Pi, through a host
  profile; a bare `pi` launch takes its first prompt as an argument) writes the
  input only
  once the agent's idle prompt renders (Pi's idle editor frame, Hermes's idle
  prompt), not when it enables bracketed paste and not when the startup grace
  of a silent agent ends.
- Hermes discards terminal input queued during its start, so an initial input
  written before its prompt rendered could be lost while `session.new`
  reported success; Pi enables bracketed paste before it draws any screen.
- When the prompt does not render within `input.initial_startup_grace_ms`,
  `session.new` fails with `session_input_not_ready`, writes nothing and rolls
  the session back, as for Claude, including for clients without
  `extended_input_ready_wait`.
- Codex keeps bracketed-paste readiness: it reports idle only in its terminal
  title, which `screen_idle` does not count (#1114).
- The runtime packages guide lists the readiness of each official runtime and
  the grace that bounds it; the Pi and Hermes package guides, the sessions
  concept and the setup guide describe it.
