---
kind: changed
---
A notification store that a build cannot read safely (a newer format, an undecodable action line or a rejected `policy.json`) no longer stops the daemon. The notification RPCs answer `notifications_unavailable`, the files are left untouched, and the daemon logs the reason and the remedy. The store now carries a `notifications/format.json` marker, and a guard fails when its shape changes without a format bump.
