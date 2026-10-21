## Unreleased (2026-10-21, unordered selected versions are kept for every runtime, #1425)

- `pohunek service upgrade` and `service check` keep the selected official
  version of `claude`, `codex` and `hermes` when it cannot be ordered against the
  release's (for example `2.0` against `1.0.3`), as they do for Pi. The plan
  reports the gap `plugin_update_version_unordered` and the retained package
  with the version the release offers. A selected version that is newer and
  ordered keeps its behaviour: for a reserved runtime the release's package
  replaces it and the plan warns.
- A host profile pinned to a digest other than a kept selection stays as it is
  and is reported `kept` with the new reason `pinned_differs_from_kept_selection`.
- A disabled unordered selection, or an enabled one whose package does not
  load, is left untouched (gap `official_package_not_serving`, with the repair
  step for the second), never replaced or enabled. `pohunek setup`
  prints the retained-package line as `service check` does.
- The profile scan behind the upgrade plan is sorted by name, independent of the
  order the filesystem returns the agents directory in (#1413).
- Docs: `install.md`, `public-api.md` and the update runbook.
