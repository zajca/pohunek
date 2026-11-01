---
kind: fixed
---
A package runtime that reports a notification under its own id now ranks as a hook provider, so its attention report upgrades the daemon's record of the same moment instead of creating a second one. The daemon marks such a record with the new `hook_admitted` field, and the notification store moves to format 2: a daemon of this version reads format 1 stores unchanged, and an older daemon that reads a format 2 store ignores the field and ranks those records by provider name.
