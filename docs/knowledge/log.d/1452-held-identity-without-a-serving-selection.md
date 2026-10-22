## Unreleased (2026-10-22, release package identity recorded beside no serving selection, #1452)

- `pohunek service upgrade`, `service check` and `pohunekd ensure-official-runtimes`
  no longer plan an install that fails with `package_identity_installed` when the
  registry records the release package's id and version under another digest and no
  enabled, loadable official selection serves the runtime. Nothing selected, a selected
  package whose root is missing or does not load, or a non-official selection leaves the
  runtime with the gap `official_package_not_serving` (exit 0, nothing written); a selected
  package with an altered root is refused with `plugin uninstall --remove-modified`. For a
  reserved runtime that is used and whose recorded package nothing pins, the gap names
  `plugin uninstall <package> --digest <digest> --yes`, after which the next upgrade installs
  the release's archive. No wire type changes.
- Docs: `public-api.md` (the holder table), `install.md` and the update runbook.
