---
kind: fixed
---
`notification.create` now refuses a report from a hook provider (`shell`, `codex`, `claude`, `hermes`, `muse`) for a session id the daemon does not hold with `hook_not_admitted`, so no provider-ranked record is stored for an id the daemon cannot check against a hook schema. Reports from other producers, and identity reports for unknown sessions, are unchanged.
