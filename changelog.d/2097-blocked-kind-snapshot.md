---
kind: added
---
`session list`, `session inspect` and the session snapshot (`SessionInfo.blocked_kind`) show what a blocked agent waits for (`permission`, `question`, `auth`, or `unspecified` for a status record that names no kind); `session list` reads `blocked/<kind>` in the ACTIVITY column. A notification projector that lags and resyncs from a snapshot keeps the `reason` of the attention record, and a blocked record without a kind no longer keeps the attention record open after an `idle` record.
