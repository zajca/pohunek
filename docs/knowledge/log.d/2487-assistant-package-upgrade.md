## Unreleased (2026-11-21, assistant package after an upgrade, #2487)

- The release archive does not carry the assistant context package, so
  `pohunek service upgrade` leaves it at its old version. The upgrade report
  now carries `assistant` (`current`, `outdated`, `absent`, `unselected`,
  `unchecked`) and an `outdated` package is reported with `pohunek setup`, which
  installs the version the plugin index lists. `pohunek assistant` prints a note
  (`knowledge_bundle_note` in `--json`) when the launched bundle is older than
  the CLI's release.
