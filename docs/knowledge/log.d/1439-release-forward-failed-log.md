## Unreleased (2026-10-22, release forward failure log, #1439)

- One failed forward of `session.release_agent` logs one
  `session.active_identity.forward_failed` WARN, and its text holds for both
  claim kinds: the daemon ends a memory claim and leaves a claim the worker
  journaled to the worker's journal. See [sessions](concepts/sessions.md).
