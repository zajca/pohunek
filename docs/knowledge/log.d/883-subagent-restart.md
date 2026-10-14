## Unreleased (2026-10-12, subagent restart, Claude helper stop, main-turn snapshot and hook failure trace, #883)

- A `SubagentStart` with a newer sequence for a terminal subagent reopens its
  record: `running` with a new revision, `finished_at_ms` cleared,
  `started_at_ms` kept; the next `SubagentStop` completes it. This tracks
  Claude background agents resumed with `SendMessage` or woken by their own
  background shell. A start for a running subagent is idempotent.
- A subagent record's phase is the phase implied by the highest-sequence
  claim the worker admitted for its id, for every provider. A newer start for
  a running record or a newer stop for a terminal one is ignored but its
  sequence is persisted in the journal, so a delayed older claim cannot reopen
  or complete the record; a repeated Codex per-turn stop on a finished child
  only advances that sequence.
- `identity-subagent-v1` lists `claude` as a provider whose stop is admitted
  only after a recorded start. Claude's prompt-suggestion helper stop after
  every main turn creates no row, changes no count and evicts no history. The
  worker remembers its sequence in an in-memory set of at most 64 stops, drops
  an entry once a start for that id applies, and ignores (`stale_sequence`) a
  delayed start that is not newer than the remembered stop. A Codex stop
  without a start records a completed subagent (#916).
- The worker logs every ignored subagent claim as
  `worker.subagent.claim.ignored` (`duplicate_start`, `stop_without_start`,
  `stale_sequence`, `not_running`; INFO, hook answer `ok: true`) and every
  refused one as `worker.subagent.claim.refused` (`capacity`; WARN, hook
  answer `ok: false`).
- The Claude state hook runs on the main-agent `Stop` (a second managed `Stop`
  entry beside the notify hook) and sends the worker a `subagent_snapshot`
  claim with only the `id`, `type` and `status` of each `background_tasks`
  entry. It reads at most 1 MiB for this action (64 KiB for the others) and
  sends nothing for a `Stop` with a non-empty `agent_id` or a missing, invalid
  or over-256-entry list. `identity-subagent-v1` admits the snapshot.
- The worker reconciles the rows of the same reporting process with an older
  sequence and settles only rows whose end the snapshot proves: a running row
  missing from the snapshot whose newest listing by the reporter falls within
  its run and is older than the snapshot is `completed`, and any other missing
  running row is `completed` only when no task that can host an unlisted agent
  (any type except `shell`, `monitor` and `MCP task`) is listed and the
  reporter's newest snapshot is older than this one and newer than the row,
  because Claude builds the listing before the hook reads its clock. Only that
  newest evidence is kept, so a snapshot older than it, delivered out of
  order, settles nothing from it. A listed row,
  running or terminal, is left unchanged, so a fast agent's own stop completes
  it and a self-woken agent reopens through its own start; a completed row keeps its
  own sequence, so a resume start ordered before the snapshot still reopens
  it. It logs `worker.subagent.reconciled` (`snapshot_absent`) and
  `worker.subagent.snapshot.applied` with the task count `tasks`, the `hosts`
  flag and the row counts `listed`, `terminalized` and `deferred`. A
  never-listed row settles at the earliest at the second main-turn end after
  its start; a row whose reporter never ends another main turn stays
  `running` until the runtime ends (`lost`). Codex has no snapshot (#916).
- The sequence contract names the clock the Codex and Claude hooks read:
  `time.clock_gettime_ns(time.CLOCK_MONOTONIC)`, system-wide, because
  `time.monotonic_ns()` is defined only within one process.
- Once a Codex or Claude state hook has seen `POHUNEK_ENV=1` and a session
  id and started `python3`, it records every reason it fails to deliver a
  report as one line in the hook failure trace `pohunek-hooks.jsonl` in the
  pohunek log directory: `input_too_large`, `input_invalid`, `no_snapshot` (with
  `detail` `subagent_stop`, `tasks_missing`, `tasks_invalid` or
  `tasks_over_bound`), `worker_socket_missing`,
  `worker_instance_missing`, `agent_pid_invalid`, `agent_identity_unavailable`,
  `worker_unreachable`, `worker_timeout`, `worker_closed`, `worker_refused`,
  and `daemon_socket_missing`, `daemon_unreachable`, `daemon_timeout`,
  `daemon_closed`, `daemon_refused` and `daemon_protocol_invalid` for the
  daemon fallback. A line is one JSON object of at most 512 bytes with `ts`,
  `session`, `provider`, `action`, `reason` and an optional fixed `detail`
  code, and never carries payload content, input ids, task data, paths or
  environment values. The worker injects the reserved `POHUNEK_HOOK_TRACE_PATH`
  and `POHUNEK_HOOK_TRACE_MAX_BYTES` (1 MiB); the hook traces only when both
  are valid, writes its lines at exit under an exclusive lock that it tries
  without blocking (at most 5 attempts, 5 ms apart, else the run's lines are
  dropped), opens the file
  with `O_NOFOLLOW` and mode `0600`, writes only to an owner-private regular
  file of the user with one link, and rotates it once into
  `pohunek-hooks.jsonl.1`, so the trace holds at most 2 MiB. Tracing never
  changes the hook's exit code or output, a report delivered on its first
  route writes no line, and a hook without `python3` writes no trace.
- `POHUNEK_INTEGRATION_VERSION` is 13. `SubagentInfo` and the public wire
  protocol are unchanged; the private worker hook protocol carries
  `subagent_snapshot`. Documented in `concepts/sessions`, the `claude-package`
  and `codex-package` guides, the `debug-session-runtime` and
  `update-after-release` runbooks, the `runtime-packages` guide,
  `docs/architecture.md` and
  `docs/public-api.md` (hook schemas, `SessionInfo.subagents`,
  `subagent_state`, Provider Notification Hooks, Hook failure trace,
  Active-Agent Hook Payloads reserved environment, Claude notification hooks,
  integration version).
