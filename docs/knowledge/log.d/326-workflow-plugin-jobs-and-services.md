## Unreleased (2026-10-13, workflow plugin jobs and services, #326)

- `workflow.toml` accepts `[jobs.<name>]` (`argv`, `every_s`, optional
  `timeout_ms`) and `[services.<name>]` (`argv`); the consent review and
  `inspect` show them.
- `pohunek workflow enable` installs and starts one user service manager unit
  per job and service (systemd user units, launchd agents) of the
  `pohunek service` installation; `disable` and `uninstall` stop and remove
  them. New error codes: `workflow_service_not_installed`,
  `workflow_unit_refused`, `workflow_units_failed`, `workflow_unit_stale`,
  `workflow_unit_unknown`.
- Every unit runs the hidden `pohunek workflow __exec`, which refuses residue
  units, records `overlap_skipped` while a run is active, runs a job once its
  wall-clock `every_s` has passed, applies a service's restart backoff, never
  forwards the origin pair or the service manager's environment, and stops
  the run's process group on `SIGTERM`, then exits 0 so a unit its service
  manager stopped reads as stopped (the run is recorded `interrupted`).
  systemd plugin units use `KillMode=mixed`, so a stop signals only the
  trampoline; the final `SIGKILL` still reaches every remaining process.
  Uninstall applies its own refusals first, then disables the plugin and
  removes its units; it fails while a unit cannot be removed. Installing a
  recorded archive again never touches its units. An enabling install
  checks everything and takes `units.lock` before it records the plugin; a
  failure after that leaves it installed disabled
  (`workflow_installed_disabled`, with the failure's code as `cause`, and
  a message that says whether undoing the failed enable succeeded). A job outside the `[jobs]` bounds is
  refused at run time too.
- Each plugin with units keeps the names of its units in a unit record under
  the store root. `disable` and `uninstall` stop and remove the recorded units
  as well as the manifest's, so a plugin whose installed files fail
  verification no longer leaves running units behind; `uninstall` refuses to
  remove the plugin record while units are still recorded.
- `enable` refuses an installation recorded for another pohunek version
  (`workflow_service_not_installed`, naming both versions), since its units
  would run that version's trampoline.
- A stop request that arrives while the trampoline verifies a unit ends the run
  before its child starts. A stop request during a refused service's restart
  pacing ends the trampoline as stopped (exit 0), not as a refusal. Run records gain the kinds `job` and
  `service` and the field `backoff_ms`.
- New owner knobs `[jobs]` (`timeout_default_ms`, `timeout_max_ms`,
  `min_interval_s`, `check_period_s`) and `[services]`
  (`restart_initial_ms`, `restart_max_ms`, `stable_after_ms`).
- The architecture concept says which login units exist besides the daemon.
- `uninstall` of an id with no record and no store root is refused as
  `workflow_plugin_unknown` before any lock is taken, the same answer as an
  existing store without the record. With `--purge` the unit lock is taken in
  creating mode, so an enabling install cannot record the plugin between the
  check and the removal; a recorded plugin is still disabled and its units
  removed first.
- A failure while marking a fresh plugin enabled that may have written the flag
  disables it again before `workflow_installed_disabled` is reported.
- `enable` records a plugin's unit names before it marks the plugin enabled.
  When the unit record cannot be written, the command fails with
  `workflow_store_failed` and changes nothing: an enabled plugin whose units
  keep running stays enabled instead of being disabled.
- The unit record is bounded at three times the manifest limit (192 KiB), so the
  record of a manifest at its size limit with short service names is read.
- On macOS, `__exec` ignores an inherited `XDG_RUNTIME_DIR` when the
  installation records the default runtime root
  (`/private/tmp/pohunek-<uid>`), and leaves it out of the run's environment;
  before, every run of such an installation was refused after
  `launchctl setenv XDG_RUNTIME_DIR`.
- A service's restart backoff counts the child's own lifetime, from its spawn
  until it ends, so verification and output draining never make an
  immediately failing service look stable; a run that started no child is
  short.
- Without a recorded `pohunek service` installation, `disable` and `uninstall`
  fail with `workflow_units_failed` and keep the unit record when the service
  manager cannot be reached while units are recorded.
- Known limit on macOS: when `__exec` itself is killed with `SIGKILL`, the
  run's process group can survive the unit; a later start can run alongside it
  and `disable` and `uninstall` do not find it. The guide documents it; a
  reconcile mechanism is a follow-up.
