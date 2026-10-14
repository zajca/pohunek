## Unreleased (2026-10-14, workflow status and service uninstall guard, #326)

- `pohunek workflow status [<id>] [--json]` reports each recorded job and
  service of the installed plugins: the native state from the service manager
  (`running`, `stopped`, `failed`, or `unknown` with `state_reason`), the last
  run record, consecutive failures and, for a service, the restart backoff
  delay. It is local and read-only. The JSON shape is in `public-api.md`, the
  fields in the workflow plugins guide's Status section.
- `pohunek service uninstall` refuses with `workflow_units_installed` while any
  workflow plugin has a unit record that names units, before the service
  manager is contacted. An unreadable unit record refuses it with
  `workflow_store_failed`. The guard lives in `service::uninstall` and the
  check in `workflow::units::check_no_units`.
- Completion of `pohunek workflow status` offers the installed plugin ids.

- A supervised run refused before its manifest verified records the unit
  name it was started with, so `workflow status` counts the refusal for that
  unit; status reads the run log under the writer's lock read-only and never
  creates the lock file.
