## Unreleased (2026-10-20, daemon job PATH overrides, #1091 #1092)

On Linux the PATH of a daemon job that records none is the systemd user manager's,
replaced by an `Environment=PATH=` of the daemon unit or its drop-ins. An
`EnvironmentFile=` that sets `PATH`, `PassEnvironment=PATH`, `UnsetEnvironment=PATH`,
a masked or broken unit and drop-ins of a unit with no file are refused with
`service_manager_environment_unavailable` and a message naming the file or directive
to change. On macOS the upgrade journal records the boot identity next to the launchd
default (`launchd_boot_identity`); after a reboot the persisted configuration is used,
and a journal without the marker is used only when the persisted configuration agrees.
See the [upgrade runbook](runbooks/update-after-release.md) and `docs/public-api.md`.
