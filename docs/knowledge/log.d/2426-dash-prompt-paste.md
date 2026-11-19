## Unreleased (2026-11-19, a first prompt that starts with `-` is pasted, #2426)

- For a runtime with `prompt_arg = true`, a first prompt of `session.new` that
  starts with `-` (for example `--version` or `-p x`) is no longer appended to
  the launch argv, where an agent CLI could read it as an option. It is pasted
  after readiness like the first prompt of a `prompt_arg = false` runtime, and
  its turn is counted as a pasted prompt. A prompt that does not start with `-`
  stays the last argv element.
- New runtime feature `prompt-arg-dash-paste-v1` (0.44.0): a package that sets
  `prompt_arg = true` can list it in `requires`, so an older core refuses the
  package. See `docs/knowledge/guides/runtime-packages.md` ("First prompt
  delivery") and `docs/public-api.md` (`PackageRuntimeInfo`).
