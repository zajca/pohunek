## Unreleased (2026-11-19, a one-word first prompt is pasted, #2433)

- For a runtime with `prompt_arg = true`, a first prompt of `session.new` that
  is a single bare word (non-empty, only ASCII letters, digits, `-`, `_`, `.`
  and `:`; for example `logout`, `e` or `completion`) is no longer appended to
  the launch argv, where an agent CLI could dispatch it as a subcommand. It is
  pasted after readiness like a dash prompt, and its turn is counted as a
  pasted prompt. Composer-special words (`/compact`, `!make`, `@file`) hold a
  character outside the set and stay the last argv element, as does any
  multi-word prompt.
- New runtime feature `prompt-arg-word-paste-v1` (0.44.0): a package that sets
  `prompt_arg = true` lists it together with `prompt-arg-dash-paste-v1` in
  `requires`, so an older core refuses the package. See
  `docs/knowledge/guides/runtime-packages.md` ("First prompt delivery") and
  `docs/public-api.md` (`PackageRuntimeInfo`).
