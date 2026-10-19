## Unreleased (2026-10-19, Hook trace acknowledgement fixture, #1273)

- The hook trace success scenario checks explicit Unix-socket acknowledgements
  without depending on worker journal fsync latency; real-worker replay
  scenarios continue to check admission and persisted subagent state.
- The runtime debugging guide clarifies that a hook timeout means no answer
  arrived within the deadline, not that the worker rejected the claim.
