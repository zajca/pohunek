## Unreleased (2026-10-30, pre-field journals and the accept cap, #1741)

- A worker journal without `boot_session_id` falls back to its `boot_identity` when that is a canonical UUID (the Linux kernel boot id); the macOS `kern.boottime` value never proves a reboot.
- `session.remove_accepting_unconfirmed` accepts up to 1024 unreadable-marker processes (never signalled, one `warn` line each); the result lists the first 64 and gives the full count in the optional `accepted_unconfirmed_total`.
