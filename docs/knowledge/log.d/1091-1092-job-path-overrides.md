## Unreleased (2026-10-20, daemon job PATH overrides, #1091 #1092)

On Linux (`ExecSearchPath=` included) the PATH of a daemon job that records none is the systemd user manager's,
replaced by an `Environment=PATH=` of a drop-in of the daemon unit (the install replaces the unit file itself, so a `PATH` set only there does not count). An
`EnvironmentFile=` that sets `PATH`, `PassEnvironment=PATH`, `UnsetEnvironment=PATH`,
a masked or broken unit and drop-ins of a unit with no file are refused with
`service_manager_environment_unavailable` and a message naming the file or directive
to change. On macOS the upgrade journal records the boot identity next to the launchd
default (`launchd_boot_identity`, the boot session UUID); after a reboot the persisted configuration is used only when `user.plist` was last modified before the current boot,
and a journal without the marker also needs the persisted value to equal the journaled default.
See the [upgrade runbook](runbooks/update-after-release.md) and `docs/public-api.md`.
