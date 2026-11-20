## Unreleased (2026-11-20, a visible dialog stays blocked under screen-neutral output, #2447)

- A screen rule with `visible_blocker = true` that wins the visible screen
  keeps the session `blocked` (source `screen`) while the agent writes output
  that changes no screen text: cursor moves, cursor show/hide, SGR-only
  writes, a redraw of the same characters. Before, such a chunk skipped the
  screen rules and the bytes-mean-working fallback published `working`
  (source `process`) over the dialog, so a program with no 7501 record (Mistral
  Vibe, Hermes, Claude or Codex below their authority gates, Pi below 1.1.0)
  never read `blocked` under a blinking cursor, and its attention record
  flickered.
- `Detector::feed` re-evaluates the whole screen for a chunk that has process
  bytes, no evidence and no screen-text change, and keeps the result only when
  it is a screen-sourced `visible_blocker` `blocked`; `tick` runs the same
  evaluation, so live detection agrees with `detect-check` replay. Weak
  blockers (rules without `visible_blocker`), a title-sourced blocker alone
  and every other state keep the byte fallback. A `visible_blocker` title
  winner that outranks a screen dialog (Codex "Action Required" over its
  approval prompt, below the authority gate) is kept as the held reading when
  the screen alone, evaluated without titles and progress, still yields a
  `visible_blocker` dialog; the published source is then `osc_title`. A screen-only dialog still carries no
  `blocked_kind`.
- No protocol, store, package-format or SDK change; older cores keep the gap.
  See "Agent state detection" in `docs/architecture.md` and the
  `visible_blocker` paragraph of `docs/knowledge/guides/runtime-packages.md`.
