## Unreleased (2026-10-14, persisted active-agent origin, #1034)

- A session record now stores how its active agent became known:
  `active_agent_origin`, either `reported` (an agent named itself through a
  hook report) or `observed` (procwatch recognized its process). An identity
  the session's worker journaled is recorded as `reported`, and startup
  reconciliation restores the origin exactly, also after the worker's claim
  expired. An agent reported under the name of a host profile that matches its
  runtime (for example a `claude` profile with its own `manifest`) therefore
  keeps that profile's detection override after a daemon restart and through
  the next package refresh. It no longer falls back to the pinned definition's
  detector until the agent reports again.
- An agent reported only to the daemon socket, the hooks' fallback when the
  worker cannot be reached, is not written to the record and is restored by
  procwatch as observed after a restart.
- The field is not part of `SessionInfo`, so the client protocol is unchanged.
- Store schema 10, a no-op migration from 9. A record written before it has
  no origin and keeps the inference from the agent's name (an agent named like
  its runtime counts as observed).
