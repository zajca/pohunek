## Unreleased (2026-10-14, reconciled sessions start with their active agent's detector, #1080)

- A session the daemon adopts at startup now starts its detector with the
  configuration its restored active agent ran before the restart. A reported
  agent gets its profile resolved through the session's pin, including that
  profile's `manifest` override. An observed agent of the session's own
  runtime gets the pinned definition, and an observed agent of another runtime
  gets that runtime's definition. The launch default applies only when there
  is no active agent, when the session's own runtime is not served, or when
  the active agent's profile or runtime no longer resolves; that last case
  keeps the active agent and logs a WARN
  (`session.active_agent.detector_unresolved`).
  Previously the launch default ran until the client reported again or a
  package change refreshed the session.
- An active agent that a managed hook reports through the session's worker now
  switches the running detector by the same rules as soon as the daemon
  projects the worker's journal, and its release returns the session to its
  default configuration. Previously only a report to the daemon socket or a
  procwatch observation switched it.
