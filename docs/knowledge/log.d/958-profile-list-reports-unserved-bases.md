## Unreleased (2026-10-13, profile list reports unserved bases, #958)

- `pohunek plugin profile list` reports `builtin` only for a profile over
  `shell`, the one runtime built into the daemon. An unpinned profile whose
  base no installed package serves, such as `codex`, `claude`, `hermes` or a
  third-party runtime, is `package_not_installed`: its launches fail with
  `runtime_not_installed`. The detail names the official package install for
  an official alias, a package that serves the base otherwise, then
  `pohunek plugin profile migrate <name>`.
- When an installed package that may serve such a base cannot be loaded (its
  root or descriptor fails verification; for an official alias only that
  alias's official package, `pohunek.runtime.<alias>`, counts), the profile is `package_unavailable` and the
  detail names `pohunek plugin doctor`, which reports the fault and its
  repair.
- The `runtime-packages` guide, the `runtime-package-recovery` runbook and
  `docs/cli.md` list the state.
