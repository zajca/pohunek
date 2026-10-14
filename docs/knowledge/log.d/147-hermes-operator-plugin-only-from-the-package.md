## Unreleased (2026-10-13, Hermes operator plugin only from the package, #147)

- The CLI installs and updates the Hermes operator plugin only from the
  installed, enabled and selected official `pohunek.runtime.hermes` package. It
  carries no copy of the operator payload; the core hook reporter stays
  compiled in and is installed beside the package payload.
- Without an installed Hermes package, `pohunek integration install|update
  --agent hermes` fails with `hermes_package_not_installed` (class
  `configuration`) and changes nothing; its recovery hint names the official
  package install. `integration status` and `doctor` report the same code in
  `package_source_error`, and doctor fails `asset_source` with that hint.
  `hermes_package_source_missing` is gone: a removed recorded package is now
  `hermes_package_not_installed` with `package_source_missing=true`.
- An install an older CLI rendered from its compiled copy (marker version 1 or
  2, or version 3 with `source=embedded`) is never migrated automatically.
  Status and doctor report `hermes_package_not_installed` while no package is
  installed, `integration uninstall` removes it without a package, and
  `integration update` after the package install rewrites it with
  `source=package`.
- A macOS host has no source for the operator plugin until darwin packages are
  attested (#929).
- The regenerated operator skill changes the package's operator digest, so a
  package-backed install reports `outdated=true` once the new package is
  installed, until `pohunek integration update --agent hermes` rewrites it.
- The `hermes-operator` guide (regenerated Hermes skill, current bare-status
  wording), the `hermes-package` guide, the source map, `docs/install.md`,
  `docs/cli.md`, `docs/runbooks/hermes-operator-plugin.md`,
  `docs/migrations/hermes-operator-plugin.md`, `docs/architecture.md`,
  `runtime-packages/README.md` and `compat/hermes/README.md` describe it.
