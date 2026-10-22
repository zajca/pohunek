## Unreleased (2026-10-22, session record writers and idle-stop upgrade hint, #1484)

The removal intent and the native-recovery rollback now write the session record under the lock a `session.set_metadata` edit holds, so a metadata edit cannot be overwritten by an older snapshot of the record. The upgrade hint of `pohunek session policy set --idle-stop-after` against a daemon without the idle-stop default names this CLI's release as the upgrade target instead of a fixed release number. See `docs/cli.md`.
