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
3. Record `runtime.state`, `runtime.worker_id`, `runtime.runtime_id`,
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
  inspect `ps` for processes of that session before recovering it. A lost
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
- `conflict`: multiple or mismatched identities claim the session. Do not stop,
  unlink, or kill either candidate automatically. Preserve the job, journal,
  and socket evidence for diagnosis. `runtime_supervision_ambiguous` means the
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
  Afterward, `pohunek session rm <id>` removes a
  `runtime_supervision_ambiguous` session: it retires the worker job of the
  exact generation the record names through the service manager, which stops
  that worker and its child, requires every worker the session's journals
  record for that generation to be gone, and then deletes the logical record. It refuses, keeping the record, a
  `runtime_identity_mismatch` conflict or a record that names no worker
  generation (`session_runtime_conflict`), a retirement the service manager
  cannot complete (`runtime_supervision_unavailable`), a still-running worker
  journaled under another generation (`runtime_identity_mismatch`), and
  unreadable session journals or a journaled worker that still runs after the
  retirement, for example outside its job (`runtime_supervision_ambiguous`). Stop such a worker by hand after
  preserving the evidence, then retry the removal. Every removal also sweeps
  the processes carrying the session's runtime ownership markers and fails
  with `runtime_supervision_ambiguous`, keeping the session and its removal
  intent, while that sweep cannot confirm every marked process exited (for
  example one whose environment cannot be read); look for leftover processes
  of the session with `ps`, stop them, and retry. When unreadable same-user
  processes are the only obstacle, the error message names each as
  `pid N (start S, command `name`)` (at most eight, then `and N more`) and
  `recover` says to inspect them, end the ones that belong to the session, and
  retry; a refusal for another reason lists no processes. If you have inspected
  the listed processes and accept that they may keep running, `pohunek session
  rm <id> --accept-unconfirmed-cleanup` removes the session anyway for that one
  call: the processes are not signalled, and the result lists them as
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

After health returns, the same `worker_id`, `runtime_id`, worker generation
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

Carry the returned `runtime_id`, `runtime_generation`, and `next_offset` into a
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
sessions first.

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
