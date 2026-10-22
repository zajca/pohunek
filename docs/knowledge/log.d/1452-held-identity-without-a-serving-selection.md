## Unreleased (2026-10-22, release package identity recorded beside no serving selection, #1452, #1455, #1480)

- `pohunek service upgrade`, `service check` and `pohunekd ensure-official-runtimes`
  no longer plan an install that fails with `package_identity_installed` when the
  registry records the release package's id and version under another digest and no
  enabled, loadable official selection serves the runtime. Nothing selected, a selected
  package whose root is missing or does not load, or a non-official selection leaves the
  runtime with the gap `official_package_not_serving` (exit 0, nothing written); a selected
  package with an altered root is refused with `plugin uninstall --remove-modified`. One
  renderer builds every hint, for Pi's managed-runtime check too (#1480): it names the
  release of a profile or session pin on the obstacle, the removal of the obstacle and
  what follows (the next upgrade, or for a Pi that keeps no enabled official selection
  `plugin install`, `plugin update` when another Pi version is installed). No wire type
  changes.
- Docs: `public-api.md` (the holder table), `install.md` and the update runbook.
