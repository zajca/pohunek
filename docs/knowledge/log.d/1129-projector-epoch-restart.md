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
- A daemon restart publishes the current state of every session again. A
  session's `error` and `session_finished` records are keyed by the runtime
  generation that ended (`projector:<session>:<kind>:g<generation>`), so a
  terminal state replayed after a restart keeps its record, a record the owner
  deleted stays deleted while the store still holds its tombstone, and a
  generation that ended while the daemon was down is reported. A terminal
  record from a daemon that keyed it by epoch reports the generation it was
  created in: it covers a replayed terminal state when it was created after
  that generation started, or when the start is unknown. A session without
  runtime information keeps the epoch key.
- A session that is still blocked after a restart raises no new
  `agent_blocked` record while an unread `agent_blocked` or
  `approval_required` record of the session exists; the store is checked when
  the blocked state arrives, and an idle observation before it does not
  change that. Once the owner read or acknowledged the record, the blocked
  state is raised again; inside the attention dedupe window the store merges
  it into a read record, as it merges any repeated report of one attention
  moment.
