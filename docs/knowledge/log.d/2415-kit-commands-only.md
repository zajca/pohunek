## Unreleased (2026-11-19, kit and plugin-ci for commands-only packages, #2415)

- `pohunek-plugin-kit lint` accepts a commands-only package
  (`package-commands-v1`, no runtime, no context) through the install-time
  descriptor loader and reports `"kind": "commands"` (line
  `plugin lint ok: <package_id> <version> commands`). `smoke` installs such a
  package on an isolated `--local-only` daemon with `plugin install --sha256`
  and runs each exported command once with `host command run local`; a
  non-zero exit fails the smoke, and `--upstream` and `--script` are refused.
  The report carries `commands` instead of `activity` and `screen_lines`.
- The reusable `plugin-ci.yml` refuses `corpus-dir`, `upstream-install` and
  `smoke-script` for a context or commands-only package right after the lint,
  before any build, and runs the smoke for a commands-only package. See
  `docs/knowledge/guides/plugin-repositories.md`.
