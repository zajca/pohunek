## Unreleased (2026-11-20, bounded per-run smoke script timeout, #2446)

- `pohunek-plugin-kit smoke --script-timeout <seconds>` bounds only the
  smoke script: an integer `1..=900`, absent keeps the 60 s default. Outside
  the range, with no `--script`, or for a commands-only package, the smoke
  refuses with a named error before the rig starts. A script that outlives
  its bound fails the smoke with an error naming the script and the bound
  (`smoke script <path> did not exit within <N> s`); the passing `--json`
  report carries the effective bound as `script_timeout_s`. Every other kit
  child (install, lint commands, daemon start) keeps `COMMAND_TIMEOUT`.
- The reusable `plugin-ci.yml` takes an optional `smoke-script-timeout`
  input (string, default empty = kit default), validated to `1..=900` in the
  `Set up the package inputs` step, refused for a context or commands-only
  package like `smoke-script`, and passed as `--script-timeout` only when
  set. The smoke job's `timeout-minutes: 30` still covers the 900 s bound
  with setup headroom. See
  `docs/knowledge/guides/plugin-repositories.md`.
