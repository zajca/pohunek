## Unreleased (2026-10-18, strict manifest-idle input readiness for Codex, #1114)

- The `[input] ready` descriptor key accepts `manifest_idle` next to
  `screen_idle`: the session's initial input is written only when the
  detection manifest's current winning evidence is idle from a screen region
  or the terminal title. OSC progress, debounced activity, process bytes,
  hooks, a bracketed-paste enable and the elapsed grace never release a
  `manifest_idle` prompt; the wait runs the full configured grace and its
  timeout still fails `session.new` with `session_input_not_ready` and rolls
  the session back.
- The Codex package (now 1.2.0) declares `ready = "manifest_idle"`, with a
  `startup_update_prompt` blocker rule in
  [the Codex guide](guides/codex-package.md): Codex enables bracketed paste
  long before its composer reads input, and its startup update popup
  (`Update available` with `1. Update now` / `2. Skip` / `3. Skip until next
  version`) would have taken a prompt released by the paste signal or a stale
  title. The rule matches the measured popup at 80, 100 and 160 columns
  (repository captures under `compat/codex/screens/update-popup/`)
  and outranks the title's idle rule.
- Old descriptors stay compatible: `bracketed_paste` (the default) and
  `screen_idle` behave exactly as before, and a host profile over a
  `manifest_idle` base whose override manifest has no idle rule on a screen or
  title region keeps the strict readiness instead of downgrading to
  `bracketed_paste`, so the input is refused rather than released by a signal
  the runtime did not declare.
- A `manifest_idle` wait stays latched closed once a visible blocker is
  sighted: only idle evidence that arrives after that sighting releases the
  prompt. OSC title arrivals are ordered against screen updates inside each
  PTY chunk, so a title received before a clear cannot be mistaken for a
  later ready signal. Sighting the blocker again (a partial repaint that
  keeps it on screen, or a full-context observation before new bytes after a
  reconfigure or resize) raises the barrier anew, so a fresh title received
  while the blocker holds cannot arm future readiness. A repeated reception
  of the same idle title counts as a fresh arrival for readiness only; the
  activity classification keeps its title-change semantics, and an eligible
  fresh screen idle rule releases the same way.
- Screen freshness includes identical character redraws, observed with the
  same `vte` state machine as the terminal grid. A clear and identical composer
  repaint count as fresh screen evidence whether received together or split
  into PTY chunks. Control-only output, incomplete UTF-8 and characters ignored
  by `vt100` cannot refresh stale screen evidence. The blocker barrier still
  rejects screen output received while a title blocker holds. Screen text
  changes and character redraws affect strict input freshness; the debounced
  activity classifier retains its existing final-text-change semantics.
- This changes the `pohunek.runtime.codex` package digest; after release,
  service upgrade repins host profiles from the old digest to the released
  package digest.
