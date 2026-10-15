## Unreleased (2026-10-14, argument-conflict usage errors end with a newline, #875)

- The human usage error of `--host` together with `--all-hosts` ends with a
  newline, so the shell prompt no longer lands on the same line as the
  message. The `session wait` conflict errors share the same rendering. The
  `--json` form is unchanged.
