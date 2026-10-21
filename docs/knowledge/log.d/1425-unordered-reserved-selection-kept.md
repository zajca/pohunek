## Unreleased (2026-10-21, unordered selected versions are kept for every runtime, #1425)

- `pohunek service upgrade` and `service check` keep the selected official
  version of `claude` and `codex` when it cannot be ordered against the
  release's (for example `2.0` against `1.0.3`), as they do for Pi. The plan
  reports the gap `plugin_update_version_unordered` and the retained package
  with the version the release offers. A selected version that is newer and
  ordered keeps its behaviour: the release's package replaces it and the plan
  warns.
- The upgrade plan lists host profiles by name, independent of the order the
  filesystem returns the agents directory in (#1413).
- Docs: `install.md`, `public-api.md` and the update runbook.
