## Unreleased (2026-10-21, disabled packages are never re-enabled by an upgrade, #1427)

- Before: `pohunek service upgrade` and `service check` installed, selected and
  enabled the release's package for `claude`, `codex` and `hermes` when the
  selected official version had been disabled with `plugin disable` and was older
  or newer than the release's (a newer one was downgraded), or was the release's
  own package. They also enabled and selected the release's own package when it
  was recorded and disabled (`plugin install --no-enable`, `plugin disable`) while
  another enabled version was selected (#1432).
- After: a selected package you disabled, or the release's own package
  recorded as disabled, is left as set, whatever its version, for every
  runtime. When an enabled version still serves the runtime, it keeps serving
  (package entry and integrations stay, nothing is downgraded) and the gap says
  so. The plan reports the gap `official_package_not_serving` with
  `pohunek plugin enable <package> --digest <digest>`, lists no package, no
  integration and no profile change for the runtime, and the next upgrade
  proceeds as for any enabled package once you enable it. For an official Pi
  package the hint is the same instead of `plugin update`, which refuses a newer
  selection as a downgrade. Only the operator leaves a package selected and
  disabled: an install records the enabled flag and the selection in one
  registry commit. A disabled package whose root is missing is hinted with
  `plugin install` of its archive before the enable; one whose root is altered
  is refused with `plugin uninstall --remove-modified`.
- `pohunek setup` (the detecting plan) no longer reports
  `official_package_not_serving` for a runtime it drops: one nothing uses and
  whose program is off the search path, whatever the state of its package (an
  unselected Pi package's `plugin update` gap included).
- A host profile pinned elsewhere beside a kept unordered selection is reported
  with the precise reason (`pinned_version_unknown`, `pinned_to_non_official_package`,
  `pinned_to_other_package`); `pinned_differs_from_kept_selection` remains for an
  official version of the same package (#1431).
- Docs: `install.md`, `cli.md`, `public-api.md`, the update runbook and the Pi
  package guide.
