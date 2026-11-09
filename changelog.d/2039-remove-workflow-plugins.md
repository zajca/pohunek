---
kind: removed
breaking: true
---
Workflow plugins are removed: the `pohunek workflow` command group, the workflow store and runner, and the service manager units and hooks that ran workflow jobs and the reaction dispatcher. The daemon unit is unchanged. A host that still has workflow plugin units installed must disable them with the previous release before upgrading, because this release cannot manage them: `pohunek service check`, `upgrade` and `uninstall` refuse with `service_workflow_units_installed`, naming each leftover unit (`pohunek-<namespace>-wf-*.service` and `.timer` under the systemd user unit directory, `io.github.zajca.pohunek.<namespace>.wf.*.plist` in `~/Library/LaunchAgents`). Remove each with `systemctl --user disable --now <unit>`, delete the file and run `systemctl --user daemon-reload`, or on macOS `launchctl bootout gui/$(id -u)/<label>` and delete the file. An old workflow store directory under the state directory is ignored.
