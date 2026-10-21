## Unreleased (2026-10-19, durable public agent reports, #1101)

- A bound `session.report_agent` or `session.release_agent` (it carries `pid` and
  `monotonic_sequence`) for a live session whose worker negotiated protocol 8 is
  forwarded into the worker, which validates the subject, journals the claim and
  acknowledges after the journal write. The daemon then commits the claim through
  the projection of the worker's snapshot, so the identity, its detector
  configuration and the record survive a restart without any other record write.
  The managed Claude and Codex hooks send the bound fields on their daemon-socket
  fallback, so their fallback reports become durable too.
- `recorded: false` now also answers a report the worker refused. A report that is
  unbound, that meets a protocol 7 worker, or whose forward failed keeps today's
  memory-only behavior and answers `recorded: true` when the daemon accepts it; a
  failed forward logs `session.active_identity.forward_failed` at WARN, and each
  memory-only cause logs `session.active_identity.not_durable` once per session.
  An accepted forward whose snapshot was not read or applied in time logs
  `session.active_identity.commit_deferred` at WARN; the next sync commits it.
- A per-session commit guard and a fence on the worker's identity revision order a
  worker snapshot against a forwarded commit: a snapshot read before the commit can
  no longer replace the forwarded identity or its release. Sessions of workers
  that predate protocol 8 keep the old, unordered behavior until they end.
- A forwarded report keeps both public session fields: a report that names a
  session id and a transcript path journals both, and the activity it carries is
  applied once the worker has acknowledged the claim, and only while that claim is
  still the session's. A claim is identified by provider, pid, process start
  identity and sequence; the worker's journal is the truth for a journaled claim.
- A release that cannot be forwarded never ends a claim the worker journaled
  (`released: false`, WARN `session.active_identity.forward_failed`): the journal
  and the exit watch of the reporting process end it. A memory report replaces a
  journaled claim only from the same process (pid and start identity) with a newer
  public sequence. A release of a process that already exited is forwarded with
  the start identity its hook claimed.
- A report that names its pid but not its start identity contends with a
  journaled claim under the start identity the host reports for the pid, so a
  reporter that predates the bound fields keeps reporting. The Hermes reporter
  now sends `pid_start_identity` and `monotonic_sequence` with its activity
  reports and its release fallback. The public order of a forwarded report whose
  identity commit was deferred is kept until the sync that adopts the claim, and a
  snapshot that re-projects an unchanged claim updates the agent label the report
  carries.
- A release a worker snapshot carries for another provider or process ends nothing
  the daemon holds: a memory claim, its activity, a memory release and an observed
  claim stay. A forwardable report or release is ordered against a memory claim or a
  memory release of the same source before it is forwarded, so a delayed older one
  cannot reach the worker's journal and win. procwatch tells two journaled claims of
  one process apart by their claim key, and a forwarded report records its public
  order on the active claim and on the last report alike. A worker without identity
  revisions (protocol 7) cannot be forwarded to: its journaled claim is not held by
  the journal, and a newer public report of the same process stays the active claim
  across syncs. A replacing claim that inherits the replaced claim's reported
  activity is tracked in #1377.
- Every claim-changing public operation is checked, forwarded and committed under
  the per-session identity-commit guard, so an operation made while another one is
  in flight is ordered against the result instead of against a stale claim. The
  public order of a claim (source, label, sequence) is recorded when the claim is
  adopted, also for the release tombstone, and a claim waiting for its commit is
  held for the memory-report and release rules. The worker call of a forward and
  the snapshot read of its commit are bounded by `worker_connect_ms`; a worker
  that does not answer leaves the report memory-only. A protocol 7 snapshot of
  another process generation behind the same pid replaces a memory report.
- A memory report or release recorded while a forwarded operation waits for its
  commit is ordered after that operation: it carries the newest accepted worker
  revision as its floor, so the adoption of the operation and later projections
  of the same identity do not replace or clear it. A waiting release holds the
  journal empty for the admission of reports, and a memory claim above a waiting
  report can be released. A request that timed out before it was written leaves nothing pending. An operation whose answer timed out or was lost after the request was sent stays pending with
  an unknown revision until a newer revision is committed, and a memory claim
  recorded after it holds against the snapshot that carries it. Pending
  operations and memory claims are in memory only.
