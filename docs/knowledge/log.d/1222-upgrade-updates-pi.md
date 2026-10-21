## Unreleased (2026-10-21, service upgrade updates the Pi package, #1222)

- `pohunek service upgrade` and `service check` keep an installed official Pi
  package current. When the official Pi package is installed from the official
  catalog, selected and enabled, and the release catalog lists a package of the
  same id for this platform, the plan installs and selects the release's version
  (use reason `installed`, a new value) and keeps the older version installed.
  Host profiles pinned to the older official version are repinned; unpinned
  profiles stay unpinned.
- The upgrade never installs an absent Pi package, never enables or selects one
  the operator turned off, and never downgrades a selected version newer than the
  release's: the plan reports it as `kept`, with the new `release_version` field
  naming the release's version. A selected version that cannot be ordered against
  the release's (not semver) is kept too, with the gap
  `plugin_update_version_unordered`, the code `plugin update` reports.
- An official package of a non-reserved runtime that the release does not list,
  or that is installed but not serving, is a gap (`official_package_unavailable_for_platform`,
  the new `official_package_not_serving`) with the `pohunek plugin update`
  command. `service check` and `service upgrade` print gaps. The report layout
  stays at `report_version` 2; both additions are additive.
- Docs: `install.md`, the Pi package guide and the update runbook state the Pi
  behaviour and the manual command.
