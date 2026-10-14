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
  worker cannot be reached, is held in memory and reaches the record only with
  the session's next record write for another reason (a native reference
  report, a stop, a worker becoming unavailable or adopted again). Without
  such a write procwatch restores it as observed after a restart (#1101). A
  worker snapshot projected after such a report replaces it with the worker's
  journaled identity, also when the snapshot was read before the report
  arrived; #1101 tracks both.
- The field is not part of `SessionInfo`, so the client protocol is unchanged.
- Store schema 10, a no-op migration from 9. A record written before it has
  no origin and keeps the inference from the agent's name: an agent named like
  its own runtime counts as observed. That restores an observed nested agent
  of another runtime correctly, but a reported agent under a profile named
  like its runtime, nested or not, restores as observed until it reports
  again.
