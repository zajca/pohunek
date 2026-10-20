## Unreleased (2026-10-20, update-after-release runbook describes the hook reinstall, #1197)

The "Managed hook assets after an upgrade" section of the
[update after release](runbooks/update-after-release.md) runbook describes what
`pohunek service upgrade` does: it reinstalls the integration hooks of every
listed config home once the new daemon is ready, reports each home as
`installed`, `failed` or `gap`, and never rolls the upgrade back for a failure.
The installer wrapper and daemon start do not rewrite hooks, so
`pohunek integration doctor` (with `--profile` or `--all-profiles`) remains the
check for homes reported as a gap, homes without hooks before the upgrade, and
installs that bypassed `service upgrade`.
