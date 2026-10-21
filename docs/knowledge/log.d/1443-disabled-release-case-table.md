## Unreleased (2026-10-21, disabled release package case table, #1443)

- Docs: the prose sentence that said when the altered root of a disabled release
  package is refused is replaced by one case table in `public-api.md` ("Disabled
  release package: case table"), derived from the upgrade plan's selection logic.
  Each row is a state of the selected version (enabled and loading, enabled and
  unloadable, unordered, disabled, none) crossed with the state of the release
  package's root and with the class of runtime (`claude`/`codex`/`hermes`, Pi) and
  `--detect`. `install.md` and the update runbook point to it. No behavior changed.
- Scenarios assert every row for a reserved runtime and for Pi, and the refusal of
  a disabled release package that a third-party package shadows beside an
  unloadable Pi selection.
