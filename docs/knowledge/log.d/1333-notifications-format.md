## Unreleased (2026-10-30, notification store format marker, #1333)

- The notification store has a format marker (`notifications/format.json`, format 1). A store written in a newer format, an action line the daemon cannot decode or a rejected `policy.json` makes the notification service unavailable instead of stopping the daemon: the RPCs answer `notifications_unavailable`, the files stay as they were, and the recovery is to upgrade or to move the `notifications` directory aside. `governance.json` and `identity.json` stay fail-closed.
