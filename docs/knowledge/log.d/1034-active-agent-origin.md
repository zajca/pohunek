## Unreleased (2026-10-14, persisted active-agent origin, #1034)

- A session record now stores how its active agent became known:
  `active_agent_origin`, either `reported` (a client named it through
  `session.report_agent`) or `observed` (procwatch recognized its process).
  Startup reconciliation restores the origin exactly. An agent reported under
  the name of a host profile that matches its runtime (for example a `claude`
  profile with its own `manifest`) therefore keeps that profile's detection
  override after a daemon restart and through the next package refresh. It no
  longer falls back to the pinned definition's detector until the client
  reports again.
- The field is not part of `SessionInfo`, so the client protocol is unchanged.
- Store schema 10, a no-op migration from 9. A record written before it has
  no origin and keeps the inference from the agent's name (an agent named like
  its runtime counts as observed).
