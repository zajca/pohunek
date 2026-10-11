---
type: Concept
id: concept/sessions
title: Sessions
description: Pohunek sessions are durable logical records backed by isolated PTY workers and controlled through a restartable daemon.
source_kind: manual
intents: [debug, help, project]
---

# Sessions

A session is a durable logical record running an agent in a worker-owned PTY.
`pohunekd` owns the public API, metadata, and logical lifecycle; one isolated
`pohunek-sessiond` worker owns the live PTY generation and child process. The
CLI controls sessions through the daemon: start with `pohunek session new`,
inspect with `pohunek session inspect`, list with `pohunek session list`, send
input with `pohunek session input`, stop with `pohunek session stop`, and attach
with `pohunek attach`.

Automation can observe a managed terminal without attaching. Use
`pohunek session screen <target> --json` for one rendered snapshot,
`pohunek session detection <target> --json` for the active detector's region
previews and complete supported region-kind set,
`pohunek session read <target> --source recent --lines 100 --json` for the newest
bounded current-screen text. Current workers safely report `source_used:
"visible"` for recent, unwrapped, and detection requests because they do not
expose scrollback or soft-wrap metadata; `alternate_screen` remains truthful.
`pohunek session output <target> --json` for a bounded newest retained tail,
and `pohunek session wait <target> ... --timeout-ms <1..8000> --json` for one
bounded state/activity/terminal/output change. Continue output with the exact
`worker_instance_id`, decimal-string `runtime_generation`, and `next_offset` returned by
the previous result. A retained-history `gap` means older requested bytes were
evicted; a runtime change means discard old cursors and restart from a fresh
screen or tail. Waiting calls use dedicated connections, and their timeout is
the guaranteed waiter-slot release bound after a client disappears.

Detection manifests can read `osc_title`, `osc_progress`, `whole_recent`,
`bottom_lines(N)`, `bottom_non_empty_lines(N)`, `top_non_empty_lines(N)`,
`last_non_empty_above_prompt_box`, `after_last_prompt_marker`,
`prompt_box_body`, and `after_last_horizontal_rule`. Parameterized regions keep
their count in each diagnostic preview. Top and prompt-adjacent regions use the
same visible-grid, wide-glyph, and soft-wrap semantics as live matching. An
unknown region fails manifest parsing instead of falling back to a broader
region. A preview waits for an accepted active-agent configuration change before
it renders. If all active previews cannot fit the public response budget,
`session detection` returns `session_detection_response_too_large` instead of
closing the control connection.

Use `--input-stdin` (alias `--stdin`) with `session new`, or `--stdin` with
`session input`, when prompt text should not appear in argv. Stdin and inline
input are mutually exclusive and bounded. Hermes programmatic input rejects
terminal controls other than intentional LF and tab, and it is disabled while
Hermes is visibly blocked on owner approval. In JSON mode, stdout contains
exactly one versioned document with either `ok` or `err`; diagnostics remain on
stderr.

`pohunek session input s-01J00000000000000000000000 'Continue.' --until idle --timeout 1000`
can confirm delivery for an
agent profile whose submit framing has no delay. The daemon first validates the
whole wait contract, so zero or over-limit timeouts cannot deliver text;
duplicate `--until` values are deduplicated in first-occurrence order; omitted
targets default to `idle` and `blocked`; the timeout range is `1..8000` ms
(default 8000). The timeout is one overall deadline measured before the
per-session input gate, so gate contention, the two-fragment worker-plan
acknowledgement, and activity waiting all consume it. Every input plan preserves
the body fragment and separate submit fragment; fire-and-forget keeps the
provider delay on the body fragment. A waited request rejects blocked activity
as `session_agent_blocked` regardless of provider policy. Because the daemon
cannot revalidate activity during a worker-owned delay or safely retract text
already consumed by an arbitrary TUI, waited input rejects delayed framing with
`session_input_wait_unsupported` before any bytes are written. Zero-delay waited
input reserves the exclusive worker write first, then captures its causal
boundary immediately before the prepared atomic two-fragment plan starts. A
timeout or shutdown while reserving the worker cancels the unsent plan without
PTY bytes. After send starts, the exchange continues consuming its late
acknowledgement and holds the per-session gate to keep the shared control stream
synchronized; after the plan is sent, delivery
outcome may be unknown, so callers inspect the session and do not retry blindly. Waiting
acquires one observation waiter slot and returns exact post-submit evidence as
`activity`, `activity_source`, `runtime`, `activity_epoch`, and decimal-string
`activity_revision`. Clients deduplicate by `(activity_epoch, runtime,
activity_revision)` because a daemon reconnect changes the epoch while retaining
the worker runtime. Rapid matching transitions remain valid even when the latest
activity changes again, arrives before submit ACK, or the event receiver lags. The wait returns
`session_not_running` if that runtime exits, `session_runtime_changed` if it is
replaced, `session_input_wait_unsupported` for delayed provider framing, and
`session_input_timeout` when delivery acknowledgement or a target does not
arrive before the deadline. Rust and TypeScript
SDK helpers validate the timeout locally and fail closed with
`session_input_wait_contract_mismatch` when a daemon ignores `wait` or omits
runtime-scoped evidence. Its recovery guidance says to inspect the session rather
than blindly resending input because delivery may already have happened.
Generic typed Rust and TypeScript `Client.call` paths route waited input through
the same helper, so they cannot bypass this validation. SIGINT or SIGTERM during
a waited CLI input returns JSON code `session_input_interrupted` without a retry
hint because delivery outcome is unknown.

`pohunek session diff <target> [--base <ref>] [--json]` prints a unified diff
of a session's worktree against a base ref: raw diff text on stdout by
default, or the structured `SessionDiffResult` (`diff`, `base`, `truncated`)
with `--json`. `--base` overrides the base ref; omitted, the daemon falls back
to the worktree binding's recorded base branch, then the repository's default
branch, and always echoes whichever ref it actually used in the result's
`base` field. A session without a bound worktree fails with a typed
`session_no_worktree` error — there is nothing to diff for a plain-`cwd`
session. The diff covers tracked changes plus untracked files (rendered as
added-file diffs) and is truncated at a file boundary when it exceeds the
daemon's size cap, reported via `truncated: true`.

Session targets are host-aware. A bare session id targets the local host; a
`<host>/<session-id>` target names a specific host. Remote session creation keeps
the existing confirmation behavior: non-local starts require explicit approval,
and JSON/non-interactive remote starts require `--yes`.

Daemon-issued session IDs use the `s-<ULID>` form. They are time-sortable opaque
identifiers, not sequence numbers; clients must preserve and display them
verbatim rather than deriving ordering or lifecycle meaning from their values.
Worker runtime paths, journals, and service-manager job names are derived only
from an `s-<ULID>` ID or the numeric `s-<digits>` form of older records, with 1
to 20 digits; any other ID is rejected there.

A session can carry an optional owner-set display name. Set it at creation with
`pohunek session new --name <NAME>`, and change or clear it later with
`pohunek session rename <target> <NAME>` (or `--clear`). The name is cosmetic:
it shows in `pohunek session list`, `session inspect`, and other clients, but never
affects targeting or recovery — a session is still addressed by its id. The
daemon trims the name and rejects a control character or an over-long one. The
name is stored in the logical session record, so it survives daemon and worker
loss.

The assistant feature reuses this session lifecycle. Its opening prompt is just
initial input to a normal session, so session warnings and applied-input status
remain the source of truth for whether the agent received that prompt.

For profile-backed agents, `session.new` writes initial input to the PTY after
the agent shows an editable prompt or enables bracketed paste. An upper-bound
startup grace permits delivery when the agent stays silent. The daemon writes
the prompt body and submit key as separate fragments using the runtime's input
rules. Native hosts record the startup grace and the host submit delay in
`service.toml` under `[input]`; fresh installs use 5000 ms and 150 ms. The
submit delay applies to the runtimes whose descriptor marks their submit delay
configurable; a per-runtime value the host set is not imposed on the others.
A successful write confirms PTY delivery, not agent consumption.
The current SDK and CLI opt into the host's full configured grace for initial
input. An older client that does not send that option keeps a 500 ms silent
startup bound so its five-second response deadline remains usable.

A session can also carry owner metadata, set atomically at creation with
repeatable `pohunek session new --meta key=value` flags (split on the first
`=`, so a value may itself contain `=`; a missing `=`, an empty key, or a key
repeated across separate `--meta` flags fails before any connection is
dialed). The daemon enforces size limits on the values. The `link.*` key
family (`link.provider`, `link.kind`, `link.id`, `link.url`, `link.branch`) is
the cross-surface convention for tying a session to a work item: every client
and the launch scripts write exactly these five keys through the shared
`pohunek_prompt::link` implementation, so a link is byte-identical regardless
of which surface created the session. The daemon treats all metadata as
opaque owner-controlled strings.

Sessions created by older review flows may retain `review.source` and
`review.dispatched_at` metadata. Those keys remain opaque session metadata.

Notifications can be linked to a session through `session_id`. Provider hook
adapters attach the id when `POHUNEK_SESSION_ID` is present and shape-valid;
invalid values are dropped so the notification is still created without session
linkage. The daemon also enriches `notification.create` with current session
context when the referenced session still exists, but a notification may outlive
the session it references.

Session attention notifications use the source-independent dedupe key
`attention:<session_id>`. That lets a daemon projector `agent_blocked` record
and a provider-hook approval record refer to the same waiting-for-input moment
without sharing a producer-specific source id. Within the policy's attention
dedupe window, Codex and Claude provider records outrank daemon projector
records for the same session attention key.

Session notifications self-resolve. When the daemon observes a session enter
`working`, or the session reaches a terminal lifecycle state, the projector
acknowledges any `unread` or `read`
`agent_blocked` and `approval_required` records for that session's
`attention:<session_id>` key and any `turn_completed` records for
`turn:<session_id>`. This keeps transient waiting-for-input and completed-turn
signals from lingering as unread after the agent has resumed; other kinds such
as `error` and `session_finished` are never auto-resolved and wait for explicit
owner action. An `idle` observation alone does not resolve attention because an
approval prompt can be technically idle while still requiring owner input.

Session notifications are also debounced before they ever become visible. An
`agent_blocked`, `approval_required`, or session-scoped `turn_completed` create
carrying `attention:<session_id>` or `turn:<session_id>` is held pending in
memory by the daemon for the policy's `attention_debounce_secs` window (5
seconds by default) instead of being persisted immediately; `notification.create`
still reports `created: true` with a minted id, but the record does not appear
in `notification.list` and no `notification_created` event fires while it is
pending. If the session enters `working` or reaches a terminal lifecycle state
inside that window, the pending record is dropped entirely and nothing is ever
created — the same self-resolve edge described above, applied before the record
surfaces rather than after. Only a genuinely outstanding session signal, still unresolved once
the window elapses, is committed and broadcast. This is distinct from
`attention_dedupe_window_secs`, which merges duplicate attention reports across
producers rather than delaying when a session notification surfaces.

Unread `turn_completed` rows are bounded per session. A newer
`turn:<session_id>` record acknowledges any older unread turn for that key with
`superseded_by` pointing at the newer record, and a visible attention record for
the same session supersedes the unread turn twin because waiting-for-owner
attention includes the fact that the turn completed.

Notification policy is provider-keyed. `enabled` is the complete base per-kind
policy, while the deterministically ordered `providers` object holds complete
overrides by open provider wire name. A missing provider key falls back to
`enabled`. The old fixed `codex` and `claude` policy fields are not accepted.

The notification policy also owns automatic retention. Informational/success,
warning, acknowledged attention, acknowledged error, and archived records have
separate TTLs. Unresolved action-required and error records have no automatic
TTL. After a sweep appends deletion events, the daemon atomically compacts the
JSONL action log once its configured action threshold is reached.

Every session has an immutable launch identity: `agent` is the selected profile
name and `agent_base` is the `RuntimeRef` of the runtime backing the session (for
example `shell`, `codex`, `claude`, or `hermes`). A shell session can temporarily host a nested Codex or Claude Code
process. The daemon
now treats active nested-agent state as an evidence tripod: process facts from
procwatch are authoritative for start/stop, hooks are the fast path for rich
claims and clean release, and PTY output remains an activity signal rather than
lifecycle authority. `SessionStart` hooks report the nested agent's PID when the
provider exposes it as the hook parent, so procwatch can bind the claim exactly.
Claude `SessionEnd` sends an explicit release for prompt clean-exit clearing;
Codex has no installed session-end release because its `Stop` event is
turn-level, so procwatch remains the release backstop. Hook claims must be
backed by a live process and age out when unbound. `active_agent`,
`active_agent_base`, and `active_agent_pid` are runtime metadata for display,
filtering, and detector behavior; they do not change the launch `agent` /
`agent_base`.

Which hook reports a session accepts is decided by the hook schema of its
runtime: the descriptor names an integration handler and a hook schema, and
core keeps the schemas as a closed compiled set. The daemon passes the schema
id to the worker at initialization, the worker journals it, and both the
worker (when a hook reports) and the daemon (when it imports worker state)
check the provider, the action, the native-reference kind, the nested-agent
rule, the ancestry matcher and the subagent fields against it. The worker also
applies the schema's subagent sequence rule (a stop must be ordered after its
start; a schema without one admits no subagent record), and the daemon refuses
imported subagents whose fields or outcome the schema does not admit. Peer
binding and expiry checks are shared by every schema. The schema the worker
journaled outranks the one the pinned runtime resolves to, and reports of a
newer asset set are refused by a session on an older schema. Codex, Claude, and the shell
use the schema with subagents, Hermes the identity-only schema, and a runtime
without an integration, such as Pi, has none and accepts no hook report. The
daemon applies the same admission on the public socket, which the managed
hooks fall back to when the worker refuses a report, so a refused report cannot
be replayed there. A worker built before schema delivery journals no schema, so
the daemon validates its state with the schema of the session's pinned runtime.

Current Claude and Codex integrations separately observe provider-managed
subagents. Their `SubagentStart` and `SubagentStop` hooks report only lifecycle
metadata to the PTY-owning worker: provider, child id, optional parent id, and
optional agent type. The worker journals multiple concurrent children, assigns
monotonic decimal-string revisions, and retains bounded completed history across
daemon and client reconnects. A running child is marked `lost` when its owning
runtime terminates. This collection does not change the parent session's
`activity`, launch identity, or recovery binding. The hook validates its action
and Pohunek handshake before reading a bounded payload directly from stdin; it
does not create a payload file. Only the lifecycle fields are retained, so the
collection never contains prompts, results, messages, transcript paths, or raw
hook payloads.

Human-readable `pohunek session list` shows the running/recent child count, and
`pohunek session inspect` includes the corresponding lifecycle rows. Streaming
clients accept `subagent_state` only when its runtime id and generation match
the current session snapshot; a full list or inspect snapshot seeds state after
reconnect or native recovery.

Foreground reconciliation selects the recognized process-group leader first;
when the leader is an unidentified wrapper, it selects a recognized member of
the same foreground PGID. It never selects a nested agent by kind from another
process group. Replacing an active agent clears stale native identity metadata
and switches the detector configuration with the new agent.

A known shell foreground group suppresses descendant fallback after clearing a
process-bound nested claim, which prevents claim/clear flapping while the agent
remains in the process tree. A recent unbound hook claim remains valid until its
claim TTL because the foreground group alone does not identify that process.
Direct-launch agent sessions preserve a matching PTY-root claim.
Transient foreground probe failures retain the last-known PGID and claim. PID
reuse is distinguished by kernel process-start identity, including delayed exit
notifications from the replaced process.

The same runtime model keeps `cwd` current. A session starts with its launch
directory, then procwatch reads the cwd of the focus process on each tick: the
active nested-agent PID when one is bound, otherwise the root PTY child. OSC 7
terminal output is accepted as an immediate cwd hint, but procwatch remains
authoritative and overwrites a hint that a later read of the process cwd
contradicts. Cwd evidence is ordered by when it was observed: a procwatch read
taken before a hint arrived never replaces that hint, and a hint for the
current cwd keeps the source that set it. Each cwd
change emits `session_updated` and re-resolves project and worktree context. If
the new cwd is inside another registered active worktree, `worktree_path`,
`branch`, and project metadata move to that worktree; if it is outside every
known worktree, `worktree_path` is cleared while git `repo`/`branch` metadata is
kept when detection still finds a repository.

Every managed `SessionInfo` has a `runtime` object distinct from its agent
`state` and `activity`. Runtime state is one of `starting`, `live`,
`reconnecting`, `terminal`, `lost`, `conflict`, or `incompatible`. `worker_id`
identifies the PTY owner and `worker_instance_id` identifies one PTY generation. A
daemon restart preserves both ids. Explicit native recovery preserves the
logical session id but changes the worker and runtime ids.

`SessionInfo.capabilities.resume` and `.fork` are frozen flags derived from the
session's native-session launch spec; fork is only offered together with
resume. Clients must use them instead of guessing from the provider name. Long-lived
wire counters (`runtime_generation`, output offsets, terminal watermarks, hook
sequences, and subagent revisions) are canonical unsigned decimal strings so JavaScript clients do
not lose precision.

`lost` means the worker or host runtime is gone and the PTY cannot be
reattached. `conflict` means discovery found ambiguous or mismatched live
identity; Pohunek quarantines it and does not kill a worker automatically. A
`conflict` whose record names a worker generation is re-checked in the
background (after 1 s, doubling to at most 60 s) while a worker still answers
or its job is supervised: a pass never kills, the session stays `conflict`
while the evidence holds, a worker that becomes adoptable is adopted `live`,
and once the worker is gone and its job ended the session becomes `lost` with
`runtime_lost` (an orderly exit with a terminal journal is imported as
`terminal`). Resume-binding conflicts, journal-only conflicts, and records
naming no worker generation are not re-checked. `session stop <id>` on a
`conflict` stops the supervised job by the identity the record names, but only
after proving that the recorded generation's journal names the recorded worker
and that the service manager's job under that generation is that worker's;
otherwise it refuses (`session_runtime_conflict`, `runtime_identity_mismatch`,
`runtime_supervision_ambiguous`, or `runtime_supervision_unavailable`) and
leaves the record untouched. A stop never accepts unconfirmed cleanup, and
`session stop` of a `lost`, `reconnecting`, or `incompatible` session is
refused with `session_runtime_lost`, `session_runtime_reconnecting`, or
`worker_protocol_incompatible`. Each classification as `conflict`, `lost`,
`reconnecting`, or `incompatible` logs one WARN, `session runtime is
{runtime.state}: {reason}`, to `pohunekd.jsonl`.
`incompatible` means the worker is alive but has no compatible private protocol
version, so the daemon leaves it running. Attach, input, and resize are not
available in these degraded states, but list and inspect retain the logical
record and diagnostic `loss_reason`. After preserving diagnostic evidence, the
operator can remove a degraded logical record with `session rm`. A `lost` runtime
has ended, so removal leaves its worker job alone; a `terminal` worker only
retains its final output, so removal retires its job. A `reconnecting` or
`incompatible` worker, or a `conflict` whose reason is
`runtime_supervision_ambiguous`, cannot be reached yet may still own a live PTY,
so removal first retires that worker's job through the service manager by the
exact generation the record names, which stops the worker and its child, then
requires every worker the session's journals record for that generation to be
gone, and only then deletes the record. For another `conflict` reason, removal
first applies the same journal, job, and process identity proof as `session stop`.
A proven generation is stopped, then its logical record is removed. Removal is
refused, and the record kept so it can be retried, when the record names no
worker generation (`session_runtime_conflict`), a conflicted worker cannot be
proven to be the recorded generation (`runtime_identity_mismatch` or
`runtime_supervision_ambiguous`), the service manager cannot complete retirement
(`runtime_supervision_unavailable`), a worker journaled under another generation
still runs (`runtime_identity_mismatch`), or the session's journals cannot be
read or a journaled worker still runs or cannot be inspected after retirement
(`runtime_supervision_ambiguous`). Every removal, whatever the runtime state,
then sweeps processes carrying a journal-proven runtime marker. A recorded
runtime absent from its worker journal is swept only when its process also
carries the removed session's ID. A process with that runtime marker but
another session ID is left alone; one with no session ID is not signalled and
keeps cleanup unconfirmed. A process whose environment the kernel withholds
(on macOS 27, Apple platform binaries such as `/bin/sh` and `/bin/zsh`) is
decided by fork lineage when the sweep knows the lost worker's creation number,
which the worker journals as the optional `worker_spawn_id`. The kernel records
the creation number of each process's creator; it keeps naming that creator after
the creator exits, until the process re-executes after being reparented, when it
is rewritten to launchd's number. A creator number equal to launchd's is
therefore ambiguous and never excludes a process. A descendant of the worker is
reaped like a marked process. A process created before the worker, or whose
chain reaches a non-launchd creator older than the worker, is ignored and not
reported. Every other hidden process stays an unreadable candidate and keeps
cleanup unconfirmed: one created after the worker by launchd, an orphan that
re-executed after its creator died, and one whose creator exited after the
worker started. The daemon uses the journaled number only when the journal was
written in the boot the host reports now, because creation numbers restart at
reboot, and several journals of one runtime bound it only when they agree. Every
journal-backed sweep passes it; the sweep of a runtime absent from its journal
passes none. A journal without it (a worker started by the previous release)
gives no lineage proof. Under the session-ID requirement a lineage-proven
process is not signalled either, since its session marker cannot be read. Linux
has no lineage proof and keeps its start-time ordering. A descendant that left the worker's process group
(macOS kills only the group) can outlive the stop and job retirement. A sweep
that cannot confirm every marked process exited fails
the removal with `runtime_supervision_ambiguous` and keeps the session listed
with its removal intent; `session rm` again, or the next daemon start,
finishes it once the leftover process is gone. When the only obstacle is
same-user processes whose environment cannot be read (so they cannot be proven
foreign to the runtime), the refusal message lists them as `pid N (start S,
command `name`)`, at most eight and then `and N more`, and its `recover` hint
says to inspect and end the ones that belong to the session before retrying.
A refusal for any other reason lists no processes.
`session rm <id> --accept-unconfirmed-cleanup` (the `session.remove_accepting_unconfirmed`
method; plain `session rm` uses `session.remove`, which always refuses) is the operator's way out when
those unreadable processes are the only obstacle. After a lost runtime on macOS
27 the runtime's own platform-binary processes are reaped without it, but it may
still be needed for hidden processes created after the worker by launchd, which
the refusal lists by pid and command name for you to judge, and for every hidden
process when the worker journaled no creation number. The flag is per call: nothing
stores it, a retried removal needs it again, and neither the reconciliation that
finishes an interrupted removal nor the retention sweep ever has it. With it the
removal proceeds past the unreadable candidates without signalling them,
logs each at `warn` once the sweep lets it proceed (before any cleanup), and lists
every one in the result as `accepted_unconfirmed_processes` (human output prints `pid N (start S, command
name)` lines after the `removed=` line, with the process-chosen command name
escaped; `--json` carries the array). A removal with more than 64 candidates is
refused before anything is deleted, so the result always fits one response. It changes
nothing else: a signalled process that is still running, a sweep error, or a
missing supervision configuration still refuses. The trade-off is that an
accepted process that does carry the runtime marker keeps running unsupervised
after the worktree, logs, and record are deleted, so inspect the listed
processes first. Web removal only calls `session.remove` and so never consent. Against
a daemon without the method the CLI reports `method_not_found` with an upgrade
hint.

Reconciliation joins the service manager's jobs with worker sockets and
journals for each worker generation. It reports `runtime_lost` when a worker's
job ended while its journal still said live, after sweeping that generation's
leftover processes (`runtime_lost_cleanup_unconfirmed` when that cleanup could
not be confirmed). A stale socket that refuses connections supplies no worker
identity: an absent job and a confirmed marker sweep yield `lost`, while a
present but silent job remains `conflict`. It reports
`runtime_supervision_ambiguous` (`conflict`) for
a present job whose worker does not answer, `runtime_identity_mismatch`
(`conflict`) for a job whose definition or process does not match the record,
and `runtime_supervision_unavailable` (`reconnecting`) while the service manager
cannot be inspected. The last three kill nothing. A session is reported `lost`
only after the service manager retired its ended job; while that retirement
fails it stays `reconnecting` with `runtime_supervision_unavailable` and is
retried. A removal interrupted by a daemon restart is finished by
reconciliation through the same steps as `session rm` (retire the recorded
generation and prove its workers gone, sweep its runtimes' marked processes,
then delete worktrees, logs, the resume binding, and the record); until those
succeed the session stays listed with `runtime_supervision_unavailable` or
`runtime_supervision_ambiguous` and is retried. A stop interrupted the same way
is replayed through its answering worker and committed `stopped` only once the
worker returns the terminal outcome; until then it stays `reconnecting` with
`runtime_supervision_unavailable` and the retry replays it. A create
interrupted before it committed is compensated from its durable create record
once its worker is proven ended (never launched, or its runtime ended): the
generation is retired first, then the worktree and its binding are removed,
then the record is deleted, and a step that fails keeps the rest for a retry. A
committed create whose `--input` prompt was not yet delivered is removed like
`session rm`, since the prompt lived only in the stopped daemon's memory.
A compensation that cannot finish while the daemon runs (a checkout that
cannot be removed, or a binding or record the store cannot drop) keeps the
session listed as `reconnecting` with `create_compensation_pending`, and the
supervision retry repeats it until the session is removed (`session_removed`).
A job of a
generation no record names is retired only once its journaled worker is proven
gone as well, even when the job itself already ended; while that worker runs or
the session's journals cannot be read, the job stays `orphaned` in the runtime
inventory (`stale_worker_generation`).

## Attach terminal behavior

`pohunek attach` uses raw terminal passthrough, preserving the terminal's native
scrollback. Ctrl-\ temporarily freezes the visible agent screen and opens a
session menu dialog whose header shows the session name, host, project,
branch (when the session has one), and live agent state. The menu owns kill
confirmation (`k` then `y`), terminate and delete (`t` then `y`, which stops the
session, removes it from the registry, and deletes its pohunek-owned worktree
checkout including uncommitted changes while keeping the branch), detach (`d`),
new session in the same worktree (`n`), fork (`f`), and rename (`r`). Agent output received while the menu is
open is buffered; closing the menu restores the frozen screen, replays that raw
output, and resumes passthrough without losing terminal modes or scroll margins.

Whenever an attach attempt ends (detach, session stop, typed failure, unexpected
EOF, or reconnect) the CLI restores normal terminal output modes after replaying
any buffered menu output. This disables mouse and focus reporting, bracketed
paste, alternate-screen state, and TUI cursor/scroll modes before returning
control to the parent shell.

Attach retries automatically after an unexpected daemon stream close. The
settings live in `<config_dir>/attach.conf` (key=value lines, `#` comments;
`pohunek setup config` installs a template with every key commented at its
default):

- `attach_reconnect_seconds` is the retry window (default 20; `0` disables retry).
- `attach_reconnect_interval_seconds` is the minimum delay between attempts
  (default 0.5).
- `attach_reconnect_max_attempts` caps consecutive attempts within the window
  (default 3), including failures where inspect still reports a running session.

The replacement daemon reconciles with the existing per-session worker, so
Codex, Claude, Hermes, and plain shell sessions retain the same PTY, child PID,
and runtime id. A typed worker-stream failure is surfaced once and is not
retried. A lost worker cannot be reconstructed by retrying attach; inspect
`runtime.state` and use explicit native recovery only when supported.

## Retention

A host that runs agents for weeks accumulates logical records for sessions that
can never be attached again, and those records keep owning their worktrees. The
daemon therefore runs a retention sweep that ages unavailable sessions out.

The policy lives at `<data_dir>/session-policy.json` and is read with
`pohunek session policy get` (add `--json` for the machine-readable shape) and
changed with `pohunek session policy set`, which accepts `--enabled` /
`--disabled`, `--sweep-interval-secs`, `--terminal-ttl-secs`,
`--lost-ttl-secs` and `--max-removals-per-sweep`. Unspecified fields keep their
current value, and the running sweep task picks the new policy up on its next
cycle, so no daemon restart is needed.

The shipped default is deliberately conservative: sweeps are **disabled**, and
once enabled they run every 6 hours, keep a terminal (`stopped`/`done`/`failed`)
session for 30 days, keep a session whose runtime is `lost` for 90 days, and
remove at most 25 sessions per sweep. The `lost` grace period is the longer one
because such a session may still be recoverable with `session resume`.

`pohunek session retention sweep --dry-run` reports exactly what the current
policy selects without touching anything; `--apply` removes the selection, and
`--limit` lowers the cap for that one sweep. A manual sweep works whether or not
automatic sweeps are enabled and never exceeds the policy's own cap.

A sweep removes through the same path as `session rm`, so it stops a runtime
that is still live, cleans pohunek-owned worktrees, and deletes the session's
logs. It never selects an `external` session or one in `conflict` or
`incompatible` runtime state, and it never selects a session that is still live
or inside its TTL. Anything the sweep cannot classify is kept: an ambiguous
record is never a removal candidate.

Because the sweep is unattended and worktree removal is forced, a session whose
age matched the policy is still **held** when its pohunek-owned worktree holds
work: an uncommitted change to a tracked file, an untracked file, commits
contained in no other branch, remote branch or tag, or a checkout whose state git
cannot report at all. A held session is reported with a `hold` reason
(`worktree_uncommitted`, `worktree_untracked`, `worktree_unpushed`,
`worktree_unknown`), counted in `held` rather than `eligible`, and logged — its
worktree stays on disk. Ignored files are not a hold, since they are ignored on
purpose and regenerated. One sweep inspects at most 64 checkouts; a matched
session past that bound is held as `worktree_unknown` and inspected by the next
sweep, because an uninspected checkout is an unproven one. `session rm` is unaffected: an explicit operator removal
still removes a dirty worktree, which is how a held session is cleaned up once
the operator has looked at it.

The sweep's counters are exact. `worktrees_cleaned` counts only checkouts
confirmed gone from disk; when `git worktree remove` fails the session record is
still evicted, and the leftover directory is reported as `worktrees_failed` and
logged instead of counted as cleaned. `pohunek session retention sweep` exits
non-zero when the sweep reports a failed removal or a leftover worktree, so a
cron job or health check sees a partial failure without parsing the output.

External observer mode is opt-in with `POHUNEK_OBSERVE_EXTERNAL_AGENTS=1` (or
`SessionRegistryConfig.observe_external_agents = true`) and defaults off because
it watches provider transcript trees under the operator's Claude/Codex homes.
When enabled, the daemon combines same-user process facts with transcript JSONL
candidates to show agents that were started outside pohunek. These entries use
synthetic ids such as `ext-12345`, carry `external: true`, and appear in
`session.list`, `session.inspect`, and other clients as read-only sessions. They have
no pohunek-owned PTY: attach, input, resize, stop, remove, rename, metadata, and
resume operations are rejected with `session_external_read_only`. The observer
removes the entry when the external process exits, including `kill -9` via the
event-driven native exit watch: a pidfd on Linux and a kqueue `NOTE_EXIT`
registration on macOS. Both re-verify the exact process identity after arming
the watch, so a reused process id never completes a watch for the process it
replaced, and a registration failure is reported as a failure rather than as an
exit.

The observer also indexes provider transcripts so a newly started external agent
is enriched without waiting for the next process sweep. It watches only the
Claude (`projects`) and Codex (`sessions`) transcript roots of the config homes
the host launches those agents with (each runtime's own home and the home of
every host profile, re-read on every pass), never their parents or `$HOME`. The transcript index is converged by a bounded reconciliation pass
that runs every 30 seconds, on a lost-event or directory-change hint, and after a
watcher restart, whether or not the live watcher (inotify on Linux, FSEvents on
macOS) is healthy. Each pass re-resolves the configured roots (symlinked or
aliased roots are scanned once, a nested root owns its own transcripts), registers
them with the watcher, and scans them, dropping indexed transcripts it no longer
sees. A pass that could not finish drops nothing. The watcher only reduces
latency: a file notification is a hint that schedules one debounced, bounded
parse of that transcript, its failure is only logged because the next pass
repairs it, and process-backed identity checks are unchanged. A stream that
silently stopped delivering therefore delays enrichment by at most one pass.

Every pass is bounded to 100000 directory entries, 8192 directories and 20000
transcript parses per root, and unchanged transcripts (same size and modification
time) are not parsed again. A provider tree beyond these bounds is visibly
degraded with `scan_incomplete`, never silently partial. The bounds apply to each
root, so a host with several account homes spends them once per distinct home.
A transcript below a root that left the set (a removed profile) leaves the index
on the next pass. Watcher health is logged
with stable machine-readable causes: `external_transcript_watcher_unavailable`
(`inotify_open_failed`, `fsevents_open_failed`, `unsupported_target`,
`watcher_backend_failed`) when no live watcher runs, and
`external_transcript_watcher_degraded` (`roots_missing`, `registration_failed`,
`scan_incomplete`) when the watcher runs but a root is missing, could not be
registered (denied directory, exhausted OS watch limit), or could not be scanned
completely (unreadable directory or a tree beyond the bounds). The registration
diagnostic names the transcript root, never the transcript contents. A failed
watcher is restarted with backoff, and the reconciliation passes and the process
sweep keep running in every state.

Workers live for the operating-system login session. Closing a terminal,
detaching, or locking the screen does not stop a session. Logging out or
rebooting ends every worker; after the next login the daemon reports those
sessions `lost` (`runtime_lost`) and never restarts or resurrects them.
Explicit `session.resume` remains the recovery path.

Detach and client restarts do not stop a session because its worker owns the
PTY. A daemon restart, daemon `SIGKILL`, or daemon binary upgrade closes client
and controller sockets, but the worker keeps the same PTY and process group,
continues draining bounded output, and accepts the replacement daemon after
reconciliation. `pohunek attach` reconnects to the same runtime id. Reconnection
emits `session_runtime_reconnected`; it does not emit `session_created`, report
child exit, or invoke native resume. Detector reconnection can replay retained
raw output from its last processed offset. A fresh interactive attach instead
applies the client's initial dimensions when known and starts from one complete
current terminal repaint, followed atomically by live output. It never rebuilds
the screen from raw bytes emitted at historical terminal sizes.
Workers negotiated below private protocol v3 cannot guarantee that ordering;
the daemon returns `attach_snapshot_unsupported`, and the session must be
restarted on an upgraded worker or forked into a new session.
Interactive attach input is ordered by a stream-scoped sequence and does not
consume the worker's bounded control-input deduplication capacity. A typed
worker stream failure is retained by the daemon for the attaching CLI, which
surfaces it instead of repeatedly treating it as an ordinary reconnect.

Native recovery metadata is accepted only from the immutable launch agent
process, so a nested different or same-provider agent cannot overwrite the
parent session's recovery reference. Managed children inherit the stable
`POHUNEK_SESSION_ID`, `POHUNEK_WORKER_ID`,
`POHUNEK_WORKER_SOCKET_PATH`, and worker hook protocol version. They also
receive `POHUNEK_DAEMON_ID` from the daemon instance that initialized their
worker. A daemon restart leaves this launch-time value in the live PTY;
`pohunek attach` rejects a session attaching to itself by matching its session
and stable worker IDs, including after the restart. The daemon also denies a
managed child request to mutate its own session, recognizing the worker's
launch-time daemon ID after a restart. Identity hooks
prefer the worker endpoint so accepted state survives daemon outage. The worker
binds every private report to the process that sends it: the connecting peer,
taken from the kernel and never from the request, must be the reported process
itself or a descendant of it. The shipped hooks already have that shape, and a
same-session sibling reporting another process is rejected. Nested
active-agent reports remain runtime evidence only: they can expose the active
agent and active native metadata while that process runs, but never populate or
replace `native_session_id` / `native_session_path` for the parent session.
Notification hooks also prefer the worker endpoint: the worker applies the same
peer binding, replaces any session id in the hook's parameters with its own, and
forwards one public `notification.create` to the stable daemon socket. A worker
that refuses or does not know the request leaves the hook to dial the daemon
socket itself, so an older worker keeps working.
Subagent lifecycle hooks use the same owner-private endpoint without a public
daemon fallback, so their durable worker state continues to advance while the
daemon or a client is disconnected.
Startup reconciliation merges the worker's immutable launch identity into the
persisted session and recovery binding; it does not replace an already captured
native reference with an empty worker field.
If the owner-private worker identity claim cannot be delivered, the shipped
hook falls back to the local public daemon with the exact runtime id, PID and
kernel start identity, a monotonic sequence, and a short expiry. That public
path carries no kernel peer binding, so it rests on those rules alone. Stale runtime,
PID reuse, wrong provider/session, expiry, and duplicate or reordered reports
are rejected. The public path is fallback; the private worker claim remains
preferred because it survives daemon outage.
Procwatch can auto-report a matching nested agent when hooks are missing, and
auto-release clears stale active fields when the backing process exits or an
unbound claim exceeds the active-agent claim TTL.

A terminal or `runtime.state=lost` session that still carries captured native
recovery metadata can be explicitly recovered with `session.resume`. The daemon
reuses the logical pohunek session id and frozen launch profile but creates a
new worker, runtime id, PTY, and child PID. Clients receive
`session_native_recovered` (including the previous and new runtime IDs when
known) and must show that generation change rather than
present it as reconnection. Recovery is rejected for live, reconnecting,
conflicting, or incompatible runtimes and is never automatic. A removed session
is gone and cannot be recovered.

Hermes uses the same durable worker model, but only for the local interactive
terminal backend in the supported 0.20.0 release. Pohunek launches it as
`hermes chat`; when a valid native reference is already present, recovery is
exactly `hermes chat --resume <reference>`. It never continues ambient Hermes
state and never reads `state.db`. Before either launch, the daemon requires an
isolated, bounded version probe to confirm the pinned release and fails with
payload-free `agent_runtime_unsupported` before material side effects when the
runtime is missing or incompatible. The Hermes operator plugin reports a native
reference through its bounded lifecycle hooks for a managed Hermes session; it
does not infer or read one from Hermes state. Its resume capability is
independent from a temporarily unavailable report; fork is always unsupported
and rejects before any child/worktree side effect. See
[Hermes operator](../guides/hermes-operator.md) for the typed tool and hook
boundaries.

Where the native reference comes from is a per-runtime strategy declared in the
runtime definition's `[native_reference]` table: `hook` (a validated integration
report, the built-in `codex`, `claude` and `hermes` behavior), `assigned` (core
generates the reference and passes it at launch) or `none` (no native recovery,
the `shell` runtime). A runtime package without an integration handler is
resumable only with `assigned`, which needs an agent CLI that accepts a
caller-chosen session id; with `hook` or `none` it gets launch and detection
only, because `session.report_native_id` accepts a report only from the launch
process itself. An assigned reference is recorded with provenance `assigned`,
is never identity evidence for ancestry, sequence or pid validation, and is
persisted before the agent starts, so the session is resumable at once. It goes
stale when the user switches conversation inside the agent (`/clear`, in-session
resume) or when the agent never wrote the conversation, so `session.resume` and
`session.fork` first run the existence check the runtime declared (a bounded,
shell-free, symlink-free file listing below a declared config home) and fail
closed with `agent_native_reference_missing` instead of launching an agent into
an empty conversation; a runtime that declares `check = "none"` is relaunched
unchecked. Recovery never falls back to another runtime, the shell or
"continue latest". A fork holds no reference of its own until its launch
process reports one, regardless of the source reference's provenance; it
cannot resume its parent's conversation. A later validated report always supersedes an
assigned reference: an assigned runtime that declares an integration handler and
hook schema may report, and the report replaces the stored value and its
provenance (`reported`) in the session, its durable record and its resume
binding, so `/clear` and in-session resume are followed and `session.resume` and
`session.fork` launch with the reported conversation. The declared existence
check runs for a still-assigned reference. A hook runtime may declare the same
kind of check under `[native_reference.existence]` for its id-kind reference;
the built-in `claude` declares one, so its recovery additionally requires a
regular transcript file under its effective declared config home; either
recovery action refuses with `agent_native_reference_missing` before
launch when the file is absent or cannot be verified. A tested worker journal
that carries a newer conversation switch the daemon cannot verify makes both
recovery actions refuse with `native_identity_uncertain` instead of resuming
the older verified target. A report must pass the usual checks (the
process-identity, sequence and expiry checks for `session.report_native_id`, the
launch process and sequence for the worker's active identity), a stale or
refused one leaves the stored reference alone, and an assigned value never
replaces a reported one, also across a daemon restart. Sequences are compared per
transport (worker claim or public report) and per runtime generation, so one
clock never makes the other stale, and a recovered generation's first claim
supersedes the reference it was relaunched with. The worker journals the latest
reference of the verified launch process apart from the active claim's lease, so
a switch made while the daemon was down survives the lease, and every
replacement carries an ordering key that decides record-versus-projection
reconciliation: the newer key wins and an unkeyed side loses to a keyed one. A
session whose runtime ended while the daemon was down imports that reference too,
when it belongs to the record's own worker instance and the verified launch
process; a report that arrives while the worker is still verifying the launch
process is promoted with the claim. The public API reference
has the field-level contract.

### Which process may report the conversation id

A hook reports the conversation id through the worker, and the worker accepts it
as the launch claim only from the launch process: the process named for the
provider that started first below the PTY root, never an interpreter such as
`node`. A process named for the provider that the launch process started
directly is accepted as well only in the provider's hook-helper role, which is
a command line whose second argument is `app-server` for Codex (it runs its
hooks from a `codex app-server` child of the launched `codex`); a provider
without a declared helper role gets no exemption. An independent same-provider
child, a deeper process, a sibling, a process of another name, and a process
whose PID or start time no longer matches are refused, so an agent a tool command starts never speaks for the session. The
first accepted claim is the initial reference. A later switch replaces it only
when the launch agent reports a newer verified conversation. Only the
`SessionStart` identity report carries a conversation
id; a subagent's `SubagentStart` and `SubagentStop` reports are a separate
claim type that never touches the reference.
The official Pi package is the worked example: see the [Pi runtime package](../guides/pi-package.md).

`session.fork` creates a new pohunek session id and PTY from the source session's
native agent conversation. The source may still be live; fork does not require a
terminal state. With `cwd_mode: "same"`, the new session starts in the source
cwd/worktree. The child begins without its own native recovery reference;
only its own verified launch-agent report can set one. The fork argv comes
from the session's frozen native-session
launch spec: Claude forks as `claude --resume <native_session_id>
--fork-session`, and a host profile that declares `fork_args` forks with exactly
those arguments. Codex fork is intentionally not enabled in this daemon contract;
Codex-backed sessions return the typed `agent_fork_unsupported` error instead
of fabricating an unsupported branch. Hermes-backed sessions return the same
typed unsupported error.

Recovery records written by earlier releases are completed when the daemon
starts, so an upgrade does not strand a resumable session. The metadata store
migration maps the flat `resume_mode`/`ref_kind`/`resumable`/`fork_*` fields of
v0.33.0 onto the session's native-session launch spec with the argv they
already produced. A v0.33.1 record that kept its native reference but lost its
launch spec is restored with the built-in spec for Claude, Codex and Hermes
(Claude regains fork); for a profile or package runtime the spec is resolved by
agent name when the record is loaded. The migration never launches anything: a
restored session stays `lost` until an explicit `session.resume`. A record that
cannot be completed (the profile was deleted, no longer runs on the same
runtime, or declares no native resume) still loads and keeps its place in
`session list`. The daemon logs `reconcile.native_recovery.unrepairable` at
WARN, the session carries a `native_recovery` warning, and `session list` prints
the hint under the table: start a new session and resume the native conversation
in it, using the reference in the warning's `detail`. Never resume the old
record when the native conversation already continues under another session,
because that would start a second agent on the same native id.

A launch (new, resume or fork) whose agent's installed Claude or Codex hook
assets are outdated (an older version marker, or content that differs from the
daemon's embedded script) carries a `hook` warning on the session: its hooks may
report nothing, so `message` names the `pohunek integration install --agent
<agent>` command (with `--profile <name>` for a host profile's home). The
session starts normally and an agent without installed hooks carries no warning.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).

## Delegated task errors

The protocol reserves a typed error contract for the delegated task layer
(`task.*` methods). The daemon does not serve those methods yet, so they answer
`method_not_found` and no task error is raised today. Every task error has a
stable `code`, a fixed `msg` and, for most, a fixed `recover` hint in the
daemon's response; none echoes a prompt, answer, path, task id or secret. The
Rust SDK's remote path prepends `host '<host>':` to `msg`. Branch on `class`
and `code` and show `msg` and `recover` verbatim. The full code table is in the Delegated Task
Errors section of `docs/public-api.md`. Sessions that belong to tasks use
`worktree_busy` (only observation is admitted while a task occupies the
worktree), `task_session_ended`, `task_fork_unsupported`, `worktree_in_use`,
`worktree_users_changed` and `task_snapshot_retired`.
