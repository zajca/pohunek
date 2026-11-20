## Unreleased (2026-11-20, smoke detection timeout, #2455)

- `pohunek-plugin-kit smoke --detection-timeout <seconds>` bounds the wait
  for the detector to leave `unknown`: an integer `1..=60`, absent keeps the
  60 s default. The range tops out at the default, so the option can only
  tighten the bounded-detection contract. Outside the range, or for a
  commands-only package, the smoke refuses with a named error before the rig
  starts. A runtime that stays `unknown` past the bound fails the smoke with
  an error naming the bound (the detector stayed `unknown` for `<N>s` after
  start); the passing `--json` report carries the effective bound as
  `detection_timeout_s`, absent for a commands-only package. The failing-
  detection scenario in `crates/plugin-kit/tests/kit.rs` runs with a 1 s
  bound instead of waiting out 60 s. See
  `docs/knowledge/guides/plugin-repositories.md`.
