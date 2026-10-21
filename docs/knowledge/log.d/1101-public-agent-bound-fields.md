## Unreleased (2026-10-19, bound agent reports on the public socket, #1101)

- `session.report_agent` accepts the optional `pid_start_identity` and
  `monotonic_sequence`, and `session.release_agent` the optional `pid`,
  `pid_start_identity` and `monotonic_sequence`. Both identities are decimal
  strings; the sequence is on the session host's `CLOCK_MONOTONIC` clock and is
  separate from the wall-clock `seq`. The change is additive: a client that
  sends none of them is served as before.
- The daemon checks a claimed `pid_start_identity` against the process the host
  reads for the `pid`. A report that names an identity without a `pid`, a
  process that no longer runs, or a process that started differently answers
  `recorded: false` and changes nothing. A confirmed identity binds the active
  claim to that process directly. A release whose process already exited is not
  checked and still applies; one whose identity the host contradicts answers
  `released: false`.
- The managed Claude and Codex state hooks send the new fields on their
  daemon-socket fallback. The daemon forwards a report that carries them into the
  worker (see the entry on durable public agent reports).
