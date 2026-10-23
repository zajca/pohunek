## Unreleased (2026-10-23, rollback after a completed 0.36 upgrade, #1549)

Rolling back from a completed 0.36 upgrade to 0.35.x is not supported: 0.36
writes `service.toml` schema 7, store schema 11 and a newer runtime catalog, and
selects official packages (Codex 1.2.2 or later, Pi 1.0.4 and, when the release carries it, Claude 1.1.0) that
0.35.x cannot read. A failed `service upgrade` still rolls back automatically.
When going back is unavoidable, restore the older packages and profile pins
under 0.36 first; the ordered procedure is in
[update after release](runbooks/update-after-release.md#roll-back-after-a-completed-036-upgrade).
