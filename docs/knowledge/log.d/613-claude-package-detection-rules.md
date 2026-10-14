## Unreleased (2026-10-12, Claude package detection rules, #613)

- The official Claude package is version 1.0.1. Its detection manifest adds to
  the built-in one: the `◐`/`◑` running-turn title and the `esc to interrupt`
  footer read `working`, so a streaming reply reads working; the question
  form reads `blocked` when its footer wraps at 30 and 40 columns; the theme
  picker, the login method list and the folder trust dialog read `blocked`;
  the completed-turn line with ` · done <time>` is the completed-turn rule; the
  npm `claude.exe` process matches, and a `node`/`bun` script that merely names
  `claude` (`claude-companion.mjs`) does not. The built-in manifest is
  unchanged. The package tests enumerate these differences from the v0.34.2
  golden. Documented in the `claude-package` guide and the source map.
