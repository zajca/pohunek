---
kind: removed
breaking: true
---
Workflow plugins are removed: the `pohunek workflow` command group, the workflow store and runner, and the service manager units and hooks that ran workflow jobs and the reaction dispatcher. `pohunek service install`, `upgrade` and `uninstall` and the daemon unit are unchanged. A host that still has workflow plugin units installed must disable them with the previous release before upgrading, because this release cannot manage them; an old workflow store directory under the state directory is ignored.
