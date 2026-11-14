## Unreleased (2026-11-14, activity cursor on list and inspect, #1318)

- `session.list` and `session.inspect` report `activity_epoch` and
  `activity_revision` of the returned snapshot once activity was recorded, so
  a buffered `agent_state` event of the same epoch whose revision is not
  greater reads as older than the snapshot. The Rust
  `SessionInfo::activity_cursor` and `agent_state_is_stale` helpers order one
  buffered event against a snapshot. See [Sessions](concepts/sessions.md) and
  `docs/public-api.md`.
