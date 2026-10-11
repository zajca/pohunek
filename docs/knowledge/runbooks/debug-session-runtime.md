---
type: Runbook
id: runbook/debug-session-runtime
title: Debug a durable session runtime
description: Diagnose worker reconnection, runtime loss, identity conflicts, and first worker-aware migration.
source_kind: manual
intents: [debug, setup, update, help]
since: 0.19.0
---

# Debug a Durable Session Runtime

Use this runbook when a session survives in the list but cannot attach, when a
daemon restart closed an attach terminal, or when an update reports a worker
problem.

Start with public, non-destructive inspection:

1. Run `pohunek health --json` and wait for the replacement daemon to become
   ready. Readiness follows worker discovery and reconciliation.
2. Run `pohunek session inspect <target> --json`.
3. Record `runtime.state`, `runtime.worker_id`, `runtime.worker_instance_id`,
   decimal-string `runtime.runtime_generation`, `runtime.last_connected_at`,
   and `runtime.loss_reason`.
4. On the session's host, run `pohunek service status --json`. Each entry in
   `workers` is one worker generation (`session_id`, `generation`,
   `service_id`, `state`, `pid`, and the proven executable and arguments). A
   session has at most one worker generation. To inspect the native job without
   changing it, derive its name from the namespace and generation: on Linux
   `systemctl --user status pohunek-<ns>-worker-<session-id>-<generation>.service`;
   on macOS `launchctl print gui/$(id -u)/io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`
   (read its exit status: `0` loaded, `113` absent; the printed text is
   informational only).
5. Inspect daemon and worker structured logs under
   `~/.local/state/pohunek/logs/`. Do not copy prompt, input, or raw terminal
   content into reports. `pohunekd.jsonl` plus seven rotations retain at most
   256 MiB; `pohunek-session-<session-id>.jsonl` plus three rotations retain at
   most 16 MiB across all worker generations for that session.

A `session.new` or fork that fails before any runtime exists reports one of
these codes instead of a runtime state:

- `worker_socket_path_invalid`: the worker socket below the daemon's runtime
  directory could never be bound. A worker first binds a staged name
  (`.s` plus 16 hexadecimal characters) beside `control.sock`, so the message
  names that longer path and the platform limit (107 bytes on Linux, 103 on
  macOS). The create is refused before anything is written or launched;
  restart `pohunekd` with a shorter `XDG_RUNTIME_DIR`.
- `worker_exited_before_ready`: the worker job ended before it accepted
  connections. The daemon stops waiting as soon as the service manager shows
  the job ended, instead of waiting for the worker connect deadline, and
  retires the job. The message carries the worker's exit status in
  `--dev-subprocess` mode and the job's final state otherwise. In
  `--dev-subprocess` mode the worker's stderr (its first 8 KiB) is logged to
  `pohunekd.jsonl` as a `worker.stderr.captured` event; for a native job read
  the systemd journal or the launchd job log instead.

Every time the daemon classifies a session runtime `conflict`, `lost`,
`reconnecting`, or `incompatible`, at startup or while it runs, `pohunekd`
logs exactly one WARN with the message template `session runtime is
{runtime.state}: {reason}` (the braces name the fields that carry the values)
and the fields `session_id`, `worker_id` (`none` when the record names no
worker), `runtime.state`, `reason`, and `detail` when there is evidence text,
for example the control-connection error behind `worker_connection_lost`.
Search `~/.local/state/pohunek/logs/pohunekd.jsonl` for `session runtime is`
and filter by `session_id`. A re-check that finds the same state and reason
logs nothing more and emits no new event; a runtime whose worker connection
dropped logs `worker_connection_lost` (`reconnecting`) once and a second WARN
when it is classified again, for example `lost` with `runtime_lost`.

Interpret runtime states as follows:

- `live`: the daemon has the current worker controller lease. Attach should use
  the existing runtime.
- `reconnecting`: a known worker is still being validated or adopted. Do not
  start native recovery or restart the worker. Reason
  `runtime_supervision_unavailable` means the service manager (systemd user
  manager or launchd) could not be inspected, or could not retire the job of a
  worker generation that is proven ended; nothing was killed and
  reconciliation retries on its own (after 1 s, doubling to at most 60 s). A
  session whose removal was interrupted (for example the daemon stopped mid
  `session rm`) also shows this reason while its removal waits for the job to
  be retired or its cleanup to succeed; the retry then finishes the removal
  and the session disappears. A session whose stop was interrupted shows this
  reason too while its stop replay fails (its worker answers but returns no
  terminal outcome yet); the retry replays the stop until the worker commits
  it, and the session then reads `stopped`. Reason
  `create_compensation_pending` marks a `session new` that failed after
  binding a worktree and whose runtime is proven ended, but whose checkout,
  worktree binding, or create record could not be removed yet (typically a
  worktree locked with `git worktree lock`, or a store write failure). Nothing
  of it runs; the same retry repeats the removal, so fix the cause (for
  example `git worktree unlock <path>`) and the session disappears with
  `session_removed` on the next pass, without a daemon restart.
- `terminal`: the worker observed child exit and the logical outcome is being
  retained or has been imported.
- `lost`: no live PTY generation remains. The logical record is intentionally
  retained; explicit native recovery is possible only with a valid launch
  recovery reference. Reason `runtime_lost` means the worker job ended while its
  journal still said live, and the ownership-marker sweep removed that
  generation's leftover processes. `runtime_lost_cleanup_unconfirmed` means the
  same, but the sweep could not confirm that every marked process ended;
  `session.resume` retries the marker sweep against the exact journaled runtime;
  it refuses with `runtime_supervision_ambiguous` and a recovery hint until
  cleanup is confirmed. Inspect and end any remaining marked processes, then
  retry. If the recorded runtime cannot be matched to its journal, recovery
  remains refused. A lost
  session without a journal for its generation reports `worker_unavailable`;
  its job is retired, but no process is swept, so check `ps` for leftovers.
  A session is reported `lost` only after the service manager retired its job;
  until then it stays `reconnecting` with `runtime_supervision_unavailable`.
  The same classification runs when a worker dies while the daemon is running:
  a proven crash is reported `lost` immediately, and a worker that stays
  unreachable is classified after the worker connect deadline (`conflict`
  while its job still runs, `reconnecting` while the manager is unavailable).
  A socket that accepts but never answers counts as unreachable: each connect
  attempt ends at the same deadline.
- A stale `control.sock` that refuses connections is only a failed endpoint
  probe. If the recorded generation's native job is absent and its journaled
  worker is proven gone, the daemon sweeps processes bearing that runtime's
  ownership marker before reporting `lost` and allowing `session resume`.
  A still-present job with a silent socket stays `conflict`; an unreadable
  journal stays ambiguous, while an unconfirmed sweep reports
  `lost` / `runtime_lost_cleanup_unconfirmed` and requires process inspection
  before recovery. Inspect the session state and service status together rather
  than treating the socket pathname or inventory reason as proof by itself.

A terminal or lost hook runtime without a trusted native reference refuses
`session.resume` before launching anything. `runtime/native_identity_missing`
means its own generation journal has no native claim;
`runtime/native_identity_unverified` means it has a claim but no verified
launch-process binding. `runtime/native_identity_evidence_unavailable` means
the durable session record or exact generation journal cannot be read, the
journal is absent or ambiguous, or its worker id or instance does not match the
record. Preserve both records for diagnosis. A foreign process claim keeps the
existing typed process-identity reason and cannot replace the stored native
reference.
`runtime/native_identity_uncertain` means the generation journal carries a
newer conversation switch that this daemon cannot verify: the journal names a
different conversation than the persisted one and no newer accepted report
covers it. Native recovery is unavailable with the older verified target
intact, because the daemon cannot tell which conversation the agent last used.
Resume the newer conversation in a fresh session with
`<agent> --resume <resolved-id>` or clear the journal discrepancy in the
session's own generation before retrying; never delete the journal.
For Claude, `runtime/agent_native_reference_missing` on resume or fork means
the selected conversation's regular transcript file could not be verified
under the effective Claude config home. Restore or inspect the transcript tree
before retrying; the refused operation creates no new worker or fork child.

- `conflict`: multiple or mismatched identities claim the session. Never stop,
  unlink, or kill either candidate by hand, and the daemon never kills one
  automatically. Preserve the job, journal, and socket evidence for diagnosis;
  `pohunek session stop <id>` is the supported exit when it can prove the
  recorded identity (see below). `runtime_supervision_ambiguous` means the
  native job is present but its worker socket does not answer and its journal is
  not terminal, or that its journals or worker socket directory could not be
  read at all. It is not permanent: the daemon re-checks it in the background
  (after 1 s, doubling to at most 60 s) without killing anything, adopts the
  worker when it answers again, and reports `lost` with `runtime_lost` once the
  job ends and the journaled worker process is gone. A create that was pending
  when the daemon restarted also shows this reason while its job is watched
  until the worker initialization deadline; the job is then retired and the
  unfinished session disappears. `runtime_identity_mismatch` means the job's definition or
  process does not match the recorded executable, session, or generation.
  A conflict whose record names a worker generation is watched whatever its
  reason, while a worker still answers or its job is supervised: adoption
  reasons (`launch_identity_*`, `active_identity_*`, subagent snapshot
  rejections, `multiple_worker_candidates`), `runtime_identity_mismatch`, and
  `runtime_supervision_ambiguous`. The first re-check runs after 1 s and the
  delay doubles to at most 60 s; a session that newly joins the watch is
  re-checked after 1 s again, whatever the delay of the others has grown to.
  Each pass classifies from fresh evidence and never kills: while the evidence holds the session stays `conflict` (no new
  event or log), a worker that becomes adoptable is adopted `live`, and once
  the worker is gone and its job has ended the session becomes `lost` with
  `runtime_lost` (`runtime_lost_cleanup_unconfirmed` when the marked-process
  sweep is unconfirmed). Every pass probes all worker sockets, not only the
  session's own: a worker is adopted only while no other socket claims the same
  session, so a shadow worker keeps the conflict (`multiple_worker_candidates`)
  until it is gone. A worker that exits in an orderly way and leaves a
  terminal journal is imported as `terminal`, not `lost`. Not re-checked, and
  needing the operator: a record that disagrees with its stored resume binding
  (`resume_binding_*` reasons), a journal-only conflict
  (`worker_journal_identity_mismatch`, a journal naming another worker than
  the record), and a record naming no worker generation.
  A live worker started by the previous release is adopted `live` after the
  store migration. Its reported launch identity is checked against the stored
  native recovery binding and fails closed only on a real contradiction: another
  provider than the session's runtime (`launch_identity_provider_mismatch`), a
  reference kind that contradicts the binding's launch spec or is stored under
  the other kind when the binding has no launch spec
  (`launch_identity_reference_kind_mismatch`), or a reference that differs from
  the stored one (`launch_identity_reference_mismatch`). A binding the migration
  could not complete (no launch spec, for example a deleted profile or a profile
  without native resume; the session carries a `native_recovery` warning) does
  not block adoption: the worker is adopted `live` and `screen`, `input`, and
  `stop` work; only resume and fork stay unavailable.
  `pohunek session stop <id>` on a `conflict` stops the supervised job by the
  identity the record names instead of refusing. Before anything is written it
  proves that the journal of the recorded generation names the recorded worker
  and that the job the service manager shows under that generation's service id
  is that worker's (executable, `--session-id` and `--worker-generation`
  arguments, and main process match the journal). It refuses with
  `session_runtime_conflict` when the record names no generation or worker,
  `runtime_identity_mismatch` when no journal of the generation names the
  recorded worker or the job is not that worker's,
  `runtime_supervision_ambiguous` when the journals are unreadable or no job holds
  the recorded worker process while it still runs (nothing could stop it), and
  `runtime_supervision_unavailable` when the job cannot be inspected. After the
  proof it persists the stop intent, retires the job of the exact generation
  (which ends the worker and its child), requires every worker journaled for
  the generation to be gone, and sweeps the processes carrying the session's
  runtime ownership markers. A sweep that cannot confirm every marked process
  exited refuses with `runtime_supervision_ambiguous` and a `recover` hint to
  inspect and end the listed processes and retry the stop; a stop never accepts
  unconfirmed cleanup (`--accept-unconfirmed-cleanup` belongs to `session rm`
  only). On success the result is `stopped: true`, the session state is
  `stopped`, the runtime `terminal`, and the stop transaction committed. A
  refusal leaves the record exactly as it was (no `desired_state = stopped`, no
  `requested` stop transaction); a retirement the supervisor reports as failed
  rolls the intent back only when the worker is proven alive beside a live job.
  Every other failure (a worker still running after the retirement, an
  unconfirmed marked-process sweep, a retirement error with the worker gone or
  unverifiable) keeps the persisted stop intent, and the watch (or the next
  daemon start) finishes the stop as `stopped` once the worker is gone and the
  marked-process sweep is confirmed; until then the session stays `conflict`
  with `runtime_supervision_ambiguous`. A stop whose record names a runtime
  (`worker_instance_id`) that the journal does not is refused with `runtime_identity_mismatch` before anything
  is written, and only journal-proven runtimes are swept. A persisted stop
  or removal intent is finished even over a `resume_binding_*` conflict, which
  quarantines adoption only, at startup and in the watch. A pending stop ends
  only once the marked-process sweep is confirmed, also when the worker left a
  terminal journal. Every watch pass judges the persisted resume binding again,
  so a quarantine whose classification could not be written stays in force,
  and an adoption or classification whose write fails keeps the session
  pending (listed as `runtime_supervision_unavailable`, with its one WARN, if
  it had no entry); the next pass writes the classification without logging it
  again. A committed stop is never turned into `lost` by a restart, although
  the retired job leaves no terminal journal (a `resume_binding_*` conflict is
  never re-checked). `session stop` on a `lost`, `reconnecting`, or
  `incompatible` session is refused before anything is written with
  `session_runtime_lost`, `session_runtime_reconnecting`, or
  `worker_protocol_incompatible`.
  `pohunek session rm <id>` removes a
  `runtime_supervision_ambiguous` session: it retires the worker job of the
  exact generation the record names through the service manager, which stops
  that worker and its child, requires every worker the session's journals
  record for that generation to be gone, and then deletes the logical record.
  For another conflict reason, it first applies the same journal, job, and
  process identity proof as `session stop`; a proven worker is stopped before
  its record is removed. It refuses, keeping the record, an unproven worker
  (`runtime_identity_mismatch` or `runtime_supervision_ambiguous`), a record
  that names no worker generation (`session_runtime_conflict`), a retirement
  the service manager cannot complete (`runtime_supervision_unavailable`), a still-running worker
  journaled under another generation (`runtime_identity_mismatch`), and
  unreadable session journals or a journaled worker that still runs after the
  retirement, for example outside its job (`runtime_supervision_ambiguous`). Stop such a worker by hand after
  preserving the evidence, then retry the removal. Every removal also sweeps
  the processes carrying the session's runtime ownership markers and fails
  with `runtime_supervision_ambiguous`, keeping the session and its removal
  intent, while that sweep cannot confirm every marked process exited (for
  example one whose environment cannot be read); look for leftover processes
  of the session with `ps`, stop them, and retry. On macOS 27 the kernel
  withholds the environment of Apple platform binaries such as `/bin/sh` and
  `/bin/zsh`; the sweep decides those by fork lineage from the lost worker's
  journaled creation number (`worker_spawn_id`, used only when the journal was
  written in the current boot). A descendant of the worker (a `/bin/zsh`
  session root and its helpers) is reaped, and a process created before the
  worker is ignored. A hidden process created after the worker by launchd (a
  launchd job or agent started later), an orphan that re-executed after its
  creator died, and a process whose creator exited after the worker started stay
  unreadable candidates, so `session rm` may still need
  `--accept-unconfirmed-cleanup` after you judge the listed processes. A runtime
  whose worker journaled no creation number (a worker started by the previous
  release) has no lineage proof, so every hidden process is a candidate. When
  unreadable same-user
  processes are the only obstacle, the error message names each as
  `pid N (start S, command `name`)` (at most eight, then `and N more`) and
  `recover` says to inspect them, end the ones that belong to the session, and
  retry; a refusal for another reason lists no processes. If you have inspected
  the listed processes and accept that they may keep running, `pohunek session
  rm <id> --accept-unconfirmed-cleanup` removes the session anyway for that one
  call (it uses the `session.remove_accepting_unconfirmed` method, so an older
  daemon answers `method_not_found`): the processes are not signalled, and the result lists them as
  `accepted_unconfirmed_processes` (`pid N (start S, command `name`)` in human
  output; the command name is escaped because the process chooses it; more than 64 candidates refuse the removal before anything is deleted). One that does carry the runtime marker keeps running unsupervised
  after the worktree, logs, and record are gone. Any other unconfirmed reason
  still refuses, and reconciliation and the retention sweep never have the
  consent. A removal finished by
  reconciliation after a daemon restart shows this `conflict` reason while it
  retries on its own.
- `incompatible`: the worker is alive but private protocol negotiation failed.
  Leave it alive and use a compatible daemon release. `pohunek session rm <id>`
  retires such a worker through the service manager the same way as an
  ambiguous `conflict` one (refused with `worker_protocol_incompatible` when the
  record names no worker generation).

Workers live only as long as the login session. Closing a terminal or locking
the screen is safe. Logging out or rebooting ends every worker job; after the
next login the daemon reports those sessions `lost` with `runtime_lost` and
never restarts them. Use `pohunek session resume <id>` to recover a session
with a native recovery reference into a new worker generation.

Restarting the daemon job is safe for workers, but it closes current public
connections. On Linux:

```bash
systemctl --user restart pohunek-<ns>-daemon.service
```

On macOS, the standard launchctl command
`launchctl kickstart -k gui/$(id -u)/io.github.zajca.pohunek.<ns>.daemon`
restarts the agent; it is not exercised by Pohunek's tests. The namespace `<ns>` is the `namespace` field of
`pohunek service status --json`.

After health returns, the same `worker_id`, `worker_instance_id`, worker generation
and PID, and agent child PID demonstrate reconnection. A new runtime id means explicit
recovery or a defect; it is not normal daemon restart behavior.

If a lifecycle or runtime operation returns
`runtime/session_runtime_commit_stale`, it lost a concurrent durable runtime
commit. Its candidate was not published to the session registry or event
stream. Run `pohunek session inspect <target> --json` again and treat the
returned runtime identity, decimal generation, and state as authoritative.
Retry only when the operation is still valid from that current runtime; do not
reuse the losing operation's runtime coordinates.

Do not interpret this error as a failed atomic rename or uncertain disk commit.
When rename succeeds but the parent-directory durability sync fails, the daemon
keeps the visible commit applied and writes a sanitized internal warning. It
does not return `session_runtime_commit_stale` for that condition.

For non-destructive terminal diagnosis, start without a historical cursor:

```bash
pohunek session screen <target> --json
pohunek session detection <target> --json
pohunek session read <target> --source recent --lines 100 --json
pohunek session output <target> --max-bytes 65536 --json
```

Use `session detection` when activity classification is surprising. Its
`supported_regions` array identifies the engine's accepted region kinds, while
`previews` shows the exact current text supplied to each region required by the
active manifest. Empty text is meaningful: for example,
`last_non_empty_above_prompt_box` is empty without a complete prompt box. The
preview waits for accepted agent/configuration changes before rendering.
`session_detection_response_too_large` means the complete diagnostic would
exceed the public response budget; reduce the manifest's number or size of
parameterized regions before retrying.

Current workers expose rendered visible rows rather than scrollback or
soft-wrap metadata. For `recent`, `recent_unwrapped`, and `detection`, verify the
truthful `source_used: "visible"` fallback and inspect `alternate_screen` before
interpreting the tail as ordinary main-screen history. ANSI reads are reserved
and fail with `session_read_ansi_unavailable`.

Carry the returned `worker_instance_id`, `runtime_generation`, and `next_offset` into a
continued output read or `session wait`. `session_runtime_changed` means the
cursor belongs to an older PTY generation; restart from a fresh screen/tail.
`session_terminal_unavailable` means the managed worker cannot currently serve
the terminal; `session_has_no_managed_terminal` identifies an external observer
entry. `worker_feature_unavailable` means a previous-version worker remains
usable for lifecycle/attach but cannot provide control-plane observation.
`session_waiter_limit_reached` is temporary bounded resource pressure; wait no
longer than eight seconds and retry instead of opening unbounded parallel waits.

Do not restart, stop, or boot out a worker job by hand. A worker job has no
restart policy (systemd `Restart=no`, launchd without `KeepAlive`) because once
a worker exits its PTY cannot be recreated. Stopping the job ends that
generation's processes and therefore loses the runtime. Use
`pohunek session stop <session-id>` for an intentional session stop.

For a Hermes runtime, first run `pohunek host inspect local --json`. The runtime
must identify `agent_base: "hermes"`, `version: "0.20.0"`, and
`supported: true` before starting a new Hermes session. A missing executable
has no version-policy fields; an installed but unparseable or wrong version is
`supported: false`. Do not diagnose by opening, copying, or editing Hermes
`state.db` or the operator's real `HERMES_HOME`. The compatibility commands use
their own temporary homes.

For the first worker-aware installation, let all legacy sessions finish or stop
them explicitly. The archive installer runs under
`pohunek service lock -- <installer>`, so it refuses before any change while
another service command holds the transaction lock (`service_transaction_in_progress`) and no such command can
start during its run. It then refuses before any change whenever
`pohunek service check --json` fails: an unusable `HOME` or XDG root, an invalid
prefix, or a prefix, unit, config, state, or runtime directory reached through
a symlink or not private to the user, each reported with the error code the
final install or upgrade would fail with. It starts a stopped legacy daemon so
the migration snapshot is always taken, moves the legacy daemon's control
socket aside, runs `migration preflight --socket <moved-socket>` there, and
lists live sessions that lack durable `runtime` metadata to refuse
replacement. Sessions with a runtime binding are already worker-owned, are
excluded from this one-time guard, and survive the daemon restart.
`packaging/install-daemon.sh --accept-runtime-loss` is destructive consent:
existing legacy PTYs cannot be transferred into workers. Use it only after
recording the affected ids and accepting that shell and uncaptured agent
sessions cannot be reconstructed. The same installer refuses to retire an
older template-unit install while any `pohunek-session@<id>.service` worker
outside the `inactive` state survives its post-stop re-check; stop those
sessions first. The flag also gates `pohunek service upgrade` of an installed
service, whose live-session preflight is described in
[update after release](update-after-release.md#upgrade-preflight).

A worker-aware daemon started over a legacy store without that preflight (for
example after replacing the binary by hand) finds legacy resume bindings but no
logical record and no manifest. It starts anyway and imports nothing: `pohunek
session list` shows none of those sessions, and `pohunek session runtime-inventory`
lists each binding as `orphaned` with reason `migration_manifest_missing`
(the daemon log carries `reconcile.migration.manifest_missing`). The bindings
stay on disk, and while they are unimported `pohunek session new` and `session
fork` fail with `migration_manifest_missing` before writing anything, because
a first logical record would make a later manifest unimportable. To recover
them, reinstall the legacy release, then rerun the worker-aware installer so
its `pohunek migration preflight` snapshots them; the worker-aware daemon it
starts imports the manifest, and session creation works again. Once any
logical record exists, a later manifest is archived without being imported.
