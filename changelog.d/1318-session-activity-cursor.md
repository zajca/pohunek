---
kind: added
---
`session.list` and `session.inspect` report an activity cursor (`activity_epoch`, `activity_revision`) of the returned snapshot once activity was recorded, so a client that buffers `agent_state` events can tell which ones the snapshot already includes: a buffered event naming the snapshot's runtime and the same epoch whose revision is not greater is older than the snapshot. ([#1318](https://github.com/zajca/pohunek/issues/1318))
