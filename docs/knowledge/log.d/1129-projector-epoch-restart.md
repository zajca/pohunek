## Unreleased (2026-10-14, projector notifications after a daemon restart, #1129)

- A daemon-projected notification (`agent_blocked`, `error`, `session_finished`
  and the other kinds the projector derives) carries the source id
  `projector:<session>:<kind>:<epoch>`. The store treats a new notification
  whose source id, provider and event match a stored record that is not
  deleted as that record, and creates nothing. The epoch restarted at 1 in
  every daemon process, so after a restart, including every upgrade, a
  session's first transitions of a kind could match records from before the
  restart that were still stored, and did not surface. The epoch of a session
  and kind now continues from the highest one the store holds; the store is
  read once per session and daemon process, off the async runtime.
- A projector epoch at the counter's limit creates nothing and logs a WARN
  instead of wrapping around to a key already in use.
