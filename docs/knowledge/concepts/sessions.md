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
and `pohunek session wait <target> --state ... --timeout-ms <1..8000> --json`
(any legacy predicate flag selects this mode) for one bounded
state/activity/terminal/output change of one session. Continue output with the exact
`worker_instance_id`, decimal-string `runtime_generation`, and `next_offset` returned by
the previous result. A retained-history `gap` means older requested bytes were
evicted; a runtime change means discard old cursors and restart from a fresh
screen or tail. Waiting calls use dedicated connections. Closing the
connection releases the waiter slots of an in-flight `session.wait`, waited
`session.output`, or `session.wait_turn` at once; the timeout stays the
guaranteed release bound for a client that vanishes without closing it.

To follow agents across turns, a client runs `pohunek session wait <target>...
[--until <kinds>] [--after <cursor>] [--timeout <duration>] --json` (turn mode,
also the mode with no flags) or calls the protocol method `session.wait_turn`
(Rust SDK `Client::session_wait_turn`, TypeScript SDK `client.sessionWaitTurn`).
It waits for the
next turn event of one or more sessions, `turn_completed`, `agent_blocked`,
`approval_required`, or `exited`, newer than an opaque cursor (or newer than
the call when no cursor is given), bounded by an agent-chosen `timeout_ms`
(daemon default 5 minutes, ceiling 30 minutes, both set by `service.toml`
`[waits]`; a longer request fails with `session_wait_limit_exceeded`). Every result carries the next `cursor` and, for
every target, `state`, `activity`, `last_output_at`, `output_advanced` (output
since the cursor), and `turn_started_at`, so a timeout still tells "still
working" from "stuck". Chain waits with the returned cursor; an event recorded
between two waits is returned by the next one. `cursor_reset` means the cursor
came from another daemon instance or lies outside what the in-memory journal retains
(also while waiting); take the fresh cursor and inspect the sessions.
`targets_ended` means every target is terminal and nothing newer is pending.
Turn events come only from admitted provider hook reports for sessions the
daemon holds and from the daemon's own projector, and are independent of the
notification policy: a `turn_completed` the inbox drops (the default) still
completes the wait. An event is recorded when the attention debounce window
(5 seconds by default) has passed since the last report, so it arrives no
earlier than that after the agent stops. A `turn_completed` whose window ends
while the session is `working` is a stale stop and is dropped; a short turn
that starts and stops inside one window is still recorded once.

The CLI turn mode takes up to 32 targets that must all be on one host (targets
on different hosts are a usage error before any request; run one wait per
host). `--until` is a comma list or repeated flag and defaults to all four
kinds; `--timeout` needs a unit (`90s`, `5m`) and defaults to the daemon
default. `--json` prints the daemon result as one document; the human form
prints the event (`<session> <kind> <at>`) or the reason, one `progress:` line
per target, and `cursor: <cursor>` last. Every reason exits 0, a timeout
included, as the legacy wait does. Mixing `--until`, `--after`, or `--timeout`
with a legacy flag is a usage error, and the legacy mode takes one target.

The cursor orders when the daemon records events, not when the agent acted.
`turn_completed`, `agent_blocked`, and `approval_required` are recorded when the
debounce window ends, and a pending `turn_completed` is dropped when the
daemon sees the session enter `working` inside the window or finds it `working`
when the window ends; `exited` is recorded
at once. The stop of the previous turn can therefore be recorded after a
cursor taken shortly after it, and a wait `--after` that cursor would return
the old stop as the end of a new prompt. Before sending a prompt, use the
cursor of the result that returned the previous turn event, or take a cursor
(`session wait <target> --timeout 1ms --json`) only once at least
`attention_debounce_secs` have passed since the previous turn ended; then send
the input and wait with `--after <cursor>`. A short wait returns the current
cursor, usually with `reason: timeout`, but it can also return an `event` or
`targets_ended`.

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
input are mutually exclusive and bounded. On a remote host, `session new --input-stdin` needs `--yes`: stdin leaves no prompt, so it fails with `confirmation_required` before reading stdin. Hermes programmatic input rejects
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

Every `session.input` write, waited or not, and the initial input of
`session.new` are bounded by the worker's input stall bound
(`input.write_stall_timeout_ms` in service.toml). When an agent stops reading
its terminal and the PTY accepts no byte for that long, the write fails with
`session_input_stalled` and the rest of that input is not written; the part
already accepted may have reached the agent, so inspect the session before
resending. For `session.new` the failed delivery rolls the new session back. Input typed or pasted
through an attached terminal is not bounded and waits like a real terminal;
while it waits, a `session.input` to the same session fails with
`session_input_stalled` after the stall bound instead of queueing behind it.

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

A runtime whose descriptor declares `[input] ready = "screen_idle"` (Claude
Code, Pi and Hermes) is ready only when its rendered prompt shows. Codex
declares the equally strict `ready = "manifest_idle"`: the detection
manifest's winning evidence must be idle from the screen or the terminal
title, so its startup update popup or a stale title cannot release the prompt
either. This applies
to initial input written into the terminal; a launch without a host profile of
a runtime that takes its first prompt as an argument (Claude, Pi, Codex) passes it on
the command line and does not wait. Claude
enables bracketed paste before its prompt box accepts input and drops earlier
pastes, Pi enables it before it draws any screen, and Hermes discards input
queued during its start, so neither the enable signal nor the end of the grace
starts the write. Such a runtime gets the full configured grace even from an
older client. When the box does not render in time, `session.new`
fails with `session_input_not_ready`, the initial input is not written, and the
session is removed through the same rollback as any failed initial-input create.

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

The `session.list` protocol request can filter this metadata with exact-value
(`metadata_eq`) and key-presence (`metadata_has`) predicates. Equality compares
the complete value; presence also matches an empty value. Metadata filters
combine with each other and existing session filters using AND semantics. Keys
and values remain opaque strings and use the same validation as
`session.set_metadata`. When a current Rust or TypeScript client targets a
protocol-3 daemon, a metadata filter fails with
`daemon/daemon_protocol_too_old` before the request is sent.

The CLI exposes these predicates as repeatable `pohunek session list
--meta-eq key=value` and `--meta-has key` options. Equality splits on the first
`=` so values can contain `=`; empty keys and values are valid. Repeated
predicates are all required to match, including contradictory values for the
same key, which produce no sessions. These options affect selection only and do
not add metadata to the human table or quiet output.

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
dedupe window, provider hook records outrank daemon projector records for the
same session attention key, and while reports are held pending in the debounce
window a provider hook record outranks a user or external record. A provider
ranks as a hook provider exactly when a registered hook schema lists it as an
identity provider (today `shell`, `codex`, `claude`, and `hermes`).

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
producers rather than delaying when a session notification surfaces. When the
window elapses the daemon also records the session turn event that
`session.wait_turn` returns, whether or not the policy keeps an inbox record for
the kind, when the report came from the projector or from an admitted provider
hook for a session the daemon holds; a create whose kind the policy disables
still answers `notification_kind_disabled`. The turn event does not use the
inbox's stale-stop suppression after resumed work: a `turn_completed` is
dropped instead when the session is `working` as its window ends.

The projector emits a `turn_completed` for every session whose activity goes
`working` to `idle`, so runtimes without managed hooks report turns too. It
reports nothing for a turn that ends `blocked`, for an idle that no `working`
preceded, or when the session works again inside the debounce window. The
daemon stamps the record with `turn_started_at`, `turn_duration_ms` and a copy
of the session metadata (`session_metadata`) that stays readable after
`session rm`; a hook report of the same turn is merged with the projector's, and
one that arrives after its turn was reported adds no second inbox record
(a waiting `session.wait_turn` may still see one more turn event). The kind stays
disabled for the inbox by default; enable it with `notifications policy set` for
a manager that follows many sessions.

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
A reporter that is not an observed agent itself, such as the Codex binary below
the `node codex.js` wrapper, or that is an observed agent below the agent the
foreground selects or below an agent launch root, keeps its claim while a scan
proves it: the process
runs now, still has the claim's start identity, is not owned by another session,
and descends from an observed agent of the same runtime, or from the launch root
of an agent session, without an agent of another runtime in between. The proof
belongs to that scan alone: a failed collection, an unreadable reporter, an exit
notification or a scan collected before the claim was admitted decides like a
claim without it, so a dead or reused PID, a sibling outside the agent's tree,
another runtime or another session's process cannot keep the claim.
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
rule, the ancestry matcher and the subagent fields against it. The worker alone
applies the schema's launch-process rule, which decides which process may set
the launch identity (the daemon holds an imported launch identity to the core
ancestry matcher only), and the schema's subagent sequence rule (a stop must be
ordered after its start, a newer start reopens a terminal record, a record's
phase follows the highest-sequence claim admitted for it, and the
providers the schema lists, `claude` in `identity-subagent-v1`, have a stop
admitted only after a recorded start; a schema without the rule admits no
subagent record), and the daemon refuses
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
runtime terminates.

For every provider, a child's phase is the phase implied by the
highest-sequence claim the worker has admitted for that id, and the journal
keeps that sequence with the record. A `SubagentStart` newer than the recorded
claim of a terminal child reopens the record: it is `running` again with a new
revision, its terminal time is cleared, `started_at_ms` keeps the time of the
first run, and the next newer `SubagentStop` completes it. Claude sends such a
start when a finished background agent is resumed with `SendMessage` or wakes
itself. A newer start for a child that is already running, or a newer stop for
a terminal child, is ignored: the phase and revision stay, but the worker
persists the claim's sequence in the journal, so a delayed older claim cannot
reopen or complete the record. A claim not newer than the recorded sequence is
ignored and changes nothing. The recorded sequence, and the sequence a
remembered orphan stop keeps, is capped at the worker's own `CLOCK_MONOTONIC`
reading when the claim arrives, as the background mark is: a claim taken from
the host's monotonic clock is never ahead of that reading, so honest claims
keep their sequence, while a far-future claim leaves a mark no later than its
arrival and the subagent's next genuine claim is still newer. Comparisons use
the claim's own sequence.

A Claude `SubagentStop` without a recorded start creates no row and leaves the
running and recent counts and the retained history unchanged; Claude sends
such a stop for its prompt-suggestion helper after every main turn, which is
not a subagent. The worker remembers the sequence of such a stop in memory
only, in a set bounded to 64 entries that drops its oldest entry beyond that,
and drops an entry once a start for that id applies. A delayed start whose
sequence is not newer than the remembered stop is ignored, so it creates no
row. A Codex `SubagentStop` without a recorded start records a completed
child, because Codex reports resumed children with stops and no start
([#916](https://github.com/zajca/pohunek/issues/916)).

At the end of every main-agent turn, Claude's `Stop` hook input lists the
backgrounded tasks that are still running or pending (`background_tasks`). The
Claude state hook forwards each task's `id`, `type` and `status`, its sanitized
`label` (from `description`), the program name of a `shell` task and the
`agent_type` of a `subagent` task, as one `subagent_snapshot` claim, identified
by the reporting process like a start and stamped with a sequence taken before
the hook reads its input. The command of a shell and its arguments are never
forwarded.
It sends no snapshot when the input exceeds 1 MiB or is not a JSON object, when
the `Stop` carries a non-empty `agent_id` (it is not the main agent's), when
`background_tasks` is missing or not a list, when the list has more than 256
entries, or when any entry lacks a non-empty string `id`, `type` or `status`
(an optional field that fails its own check is dropped alone, and the claim
still goes out);
the hook never truncates the list, and a missing list never means that nothing
runs. A snapshot is evidence, not a claim on a row's phase: it settles only
rows whose end it proves. Claude builds the listing before the hook reads its
clock, so the snapshot's sequence is only an upper bound on when the listing
was taken; the lower bound is the same reporting process's previous snapshot,
because Claude's main turns run one after another and each listing is built
after the previous `Stop` hook started. The worker considers only the rows the
same Claude process started or reopened whose recorded sequence is older than
the snapshot's, so a later start or stop always wins over the snapshot. Of the
reporter's earlier snapshots the worker keeps only the newest evidence: the
newest snapshot that listed each row and the reporter's newest snapshot. A
snapshot older than that evidence, delivered out of order, settles nothing
from it, even when a still older snapshot listed the row or preceded it:

- a running row whose id is listed stays running; the worker remembers the
  listing in memory and changes neither its sequence nor its journal record,
  so the agent's own `SubagentStop` still completes it;
- a running row that this snapshot omits and whose newest listing by the
  reporter falls within its current run and is older than this snapshot is a
  background agent that finished, and becomes `completed`;
- any other running row this snapshot omits becomes `completed` only when the
  snapshot lists no task that can host an unlisted agent (any task type except
  `shell`, `monitor` and `MCP task`) and the reporter's newest snapshot is
  older than this one and newer than the row, which proves the agent started
  before this listing was built; otherwise it is deferred and
  stays running, because Claude does not list teammates, synchronous children
  of a background agent, or workflow agents by their agent id, and an agent
  that started while the listing was being built is missing from it;
- a terminal row whose id is listed is left as it is: Claude can still list an
  agent whose own `SubagentStop` already arrived, and a self-woken agent
  reopens its row through its own `SubagentStart`, which is newer than the
  recorded stop.

A row the snapshot completes keeps its own recorded sequence, so a resume
`SubagentStart` ordered between that sequence and the snapshot's still reopens
it with a new revision. Each main turn therefore settles the Claude rows it can
prove: a background agent that a main `Stop` listed and whose own stop never
arrived holds a running-child slot only until the next main-turn end after it
finished. A reconciled row looks like any other completed row; the reason is
recorded only in the worker log. Two limits remain. A row that no snapshot
listed settles at the earliest at the second main-turn end after its start,
because the reporter's previous snapshot must be newer than the row.
A row whose reporter never ends another main turn stays `running` until its
own stop arrives or the runtime ends, when it becomes `lost`. Rows of another
reporting process, such as a second Claude in a shell session, are outside a
snapshot's scope. The reporting process and last listing of each row, and the
last snapshot sequence of each reporting process, are kept in worker memory
only. The worker remembers the last snapshot of at most
`SNAPSHOT_REPORTER_CAPACITY` (16) reporting processes and forgets the least
recently reporting one first; a forgotten process's never-listed rows stay
deferred through the reporter's next snapshot, which only records itself, and
settle at the earliest on the one after it. Codex `Stop` carries no task list, so Codex
rows are never reconciled this way ([#916](https://github.com/zajca/pohunek/issues/916)).

Every admitted subagent claim the worker ignores or refuses is logged in the
session's worker log with a reason code: `duplicate_start`,
`stop_without_start`, `stale_sequence` and `not_running` at INFO, and
`capacity` (the running-child limit) at WARN; an inadmissible claim, including
a snapshot whose task list breaks a bound, is logged as the
`worker.identity.rejected` warning. A snapshot logs one
`worker.subagent.reconciled` at INFO for each row it completes (reason
`snapshot_absent`), and one `worker.subagent.snapshot.applied` at INFO with the
number of `tasks` in the snapshot, the `hosts` flag, and the row counts
`listed` (running rows the snapshot listed), `terminalized` and `deferred`.
The background events are listed in the
[debug-session-runtime](../runbooks/debug-session-runtime.md) runbook. The worker
answers the hook with success when it admitted the claim and either applied it
or ignored it for one of the INFO reasons, and an admitted, valid snapshot
whether or not it changed a row; it answers with failure when the claim was
refused, was inadmissible, or its journal write failed. In every case the hook exits quietly, because subagent
claims have no daemon fallback. Inside a Pohunek session a state hook records
each reason it fails to deliver a report (input over the bound or invalid, no
snapshot to send, worker or daemon socket missing, unreachable, timed out,
closed or refusing) as one line with `ts`, `session`, `provider`, `action`,
`reason` and an optional fixed `detail` code, and never payload content, input
ids, paths or environment values, in the owner-private `pohunek-hooks.jsonl`
of the pohunek log directory (path from `POHUNEK_HOOK_TRACE_PATH`, rotated
once into `.1` at `POHUNEK_HOOK_TRACE_MAX_BYTES`, so at most twice that in
total), and still exits 0; the reason codes are in the
[debug-session-runtime](../runbooks/debug-session-runtime.md) runbook.

This collection does not change the parent session's
`activity`, launch identity, or recovery binding. The hook validates its action
and Pohunek handshake before reading a bounded payload directly from stdin (at
most 64 KiB, or 1 MiB for the main `Stop`, whose input also carries the final
assistant message and each task's description and command); it does not create a payload
file. Subagent lifecycle rows keep lifecycle fields only: a snapshot's task
`type` and `status` decide the reconciliation and are not stored in the rows.
Task text is kept only in the sinks that hold the background snapshot. Each of
them holds a task's sanitized `label` and a shell's `program`, and nothing else
of the task: the worker journal (the snapshot below), `metadata.jsonl` (the
`info.background` of the session record), the daemon event log under `events/`
(the `session_updated` and `background_state` payloads), and the protocol
surfaces clients read, which reach remote clients through the relay. No command
or argument of any task is kept in any of them, and no prompt, result, final
message, transcript path or raw hook payload is kept anywhere. The worker and
daemon logs record the number of items and their kinds, never a label or a
program.

The current privacy rule for background tasks is the operator decision of
2026-10-12 in [#890](https://github.com/zajca/pohunek/issues/890). It exposes a
task's sanitized label and a shell's program name to clients, including over
the relay, and never its command or arguments. That rule revises the rule that
only lifecycle fields are retained, which the subagent collection above still
follows for its rows.

The worker keeps the latest background snapshot of the session's main agent
(`background` in the worker inspect snapshot), replaced as a whole by each
applied turn-end snapshot. Each item holds `id`; a closed `kind` (`subagent`,
`workflow`, `shell`, `monitor`, `mcp_task`, `teammate`, `dream`,
`auto_mode_scan`, `memory_import`, `cloud_session`, or `other` with the
provider's own type label in `provider_kind`); a closed `status` (`running`,
`pending`, or `other`); the `agent_type` of a subagent; the `label`; the
`program` of a shell; `first_seen_ms`, the first snapshot that listed the item,
which is not a start time because Claude reports none; and `last_seen_ms`. The
label is the task description with every control, separator and format
character removed, whitespace runs collapsed to one space, trimmed, and cut to
`BACKGROUND_LABEL_MAX_BYTES` (160) bytes on a character boundary. The program
is the basename of the first command word, after leading `NAME=value`
assignments, an `env` prefix and `cd <dir> &&` or `;` prefixes; it is kept only
when it matches `[A-Za-z0-9][A-Za-z0-9._+-]*` within
`BACKGROUND_PROGRAM_MAX_BYTES` (64) bytes, and otherwise dropped. The command
and its arguments are never stored, journaled, served or logged.

Snapshots are ordered by sequence, not by identity. A snapshot from any
admitted, process-valid report replaces the stored one when its sequence is
newer than the sequence mark, which is the highest sequence of an applied
background snapshot or of a clear (see the clearing rules below); an older one
is ignored with reason `stale_sequence`. The reporting process becomes the snapshot's owner, a private
journal field (pid and start identity) that never reaches the inspect snapshot
or the logs. Every applied snapshot takes a new revision, even when its items
are unchanged, so a client sees the refreshed `observed_at_ms`. An item keeps
its `first_seen_ms` only when the same process reported it before. A snapshot
item of kind `subagent` changes no subagent row. The subagent reconciliation
above applies to every snapshot unchanged, whichever process reported it.

Who may report is the same trust boundary as subagent claims: any process in the
managed PTY tree whose ancestry the schema admits, such as the agent or a
Claude it started. The active identity is not consulted, so the background
does not depend on which agent last reported an identity.

Nested Claude sessions have one trade-off. While two Claude processes are live
in one session, the latest turn end wins, so the outer agent's background can
be replaced by the nested one's. When the nested owner releases, its snapshot
is cleared and the background is empty until the outer agent's next turn end
restores it. A background can therefore stay empty for as long as the outer
agent runs no turn, which is the price of not scoping by identity.

A snapshot listing more than `BACKGROUND_MAX_ITEMS` (64) tasks does not replace
the background; it is ignored with reason `too_many_items`, while the
reconciliation still applies it under its own 256-task bound. A hook input over
1 MiB sends no snapshot at all, so the previous background stays, and its
`observed_at_ms` shows its age.

The worker checks each optional field again. A label or program that fails its
check, an optional field of the wrong JSON type, and an `agent_type` on a
non-subagent or a `program` on a non-shell (a kind mismatch) are dropped alone,
counted by reason, and never logged with their values; the rest of the snapshot
applies. The limits are configuration in `pohunek-worker-protocol`. The worker
injects the two byte limits the hook needs into the agent environment as
`POHUNEK_BACKGROUND_LABEL_MAX_BYTES` and `POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES`,
and the hook forwards no label and no program when either is missing or
invalid.

A snapshot belongs to the agent process that reported it, so the background is
cleared in these cases:

- the runtime ends (`runtime_end`);
- the owner releases its session (`release`), unless the hook forwards the
  release reason `clear`: a `/clear` ends the session and starts it again in
  the same process, so the snapshot stays. The release reasons the hook
  forwards are `clear`, `resume`, `logout`, `prompt_input_exit` and `other`,
  and every one of them except `clear`, as well as an absent
  reason clears. The release is matched to the owner whether or not it also
  ends the active identity, so a later release of the owner still clears after
  a `clear` release whose session start never arrived. A release from another
  process changes nothing;
- an admitted, process-valid identity report comes from a process other than
  the owner (`agent_changed`), whatever its session source: when the report is
  accepted as the active identity, whatever its sequence, because sequences of
  different processes are never compared, and then the stored sequence is
  raised to the worker's own `CLOCK_MONOTONIC` reading, so an owner snapshot
  taken before the report but delivered after it stays stale; when the identity
  rules refuse it, only if the stored sequence is older than the report's;
- an admitted, process-valid identity report from the owner with source
  `startup` or `resume`, ordered after the stored sequence (`session_start`);
- the owner process is not running (`owner_exited`): it has exited and been
  reaped, it is a zombie that was not reaped, or its pid now names a process
  with another start identity. The worker checks this with the platform's
  `is_running`, which rejects zombies and mismatched start identities. The
  inspect path runs the check on every inspect, which the daemon polls every
  100 ms, and the clear is persisted before the inspect answers. A process that
  cannot be read keeps the snapshot, since the owner may still be alive. A killed
  agent sends no release, so this check is what ends its snapshot.

A `compact` session of the owner, or a `clear` session after a `clear` release
of the owner, keeps the background.

A release clear and a session-start clear raise the sequence mark to the
sequence of the release or report that caused them. A claim ordered before that
point is stale, even when it arrives late and was begun before the clear. This
is what keeps a hook that began before a release from restoring the snapshot the
release ended. An owner-exit clear does not raise the mark: a late claim from the
dead owner is already refused, because its process must still be alive with its
start identity, and raising the mark would refuse a new owner's Stop whose hook
read its clock just before that inspect. A runtime end needs no mark, because
the runtime's claims end with it.

Claim sequences are not checked against the worker's clock: every claim kind is
admitted and ordered by its own sequence. The background mark is the one
exception that uses the clock. Every write of the mark is clamped to the
worker's own `CLOCK_MONOTONIC` reading at receipt, so the mark never exceeds
the clock. A runtime package may keep a clock of its own whose readings run
ahead of the worker's. Without the clamp, one such reading would make every
later genuine snapshot stale until the runtime ends. With it, a far-future
snapshot still applies, and the next normal snapshot, ordered after it, replaces
it. The comparisons keep the unclamped sequence. A genuine snapshot whose hook
started before a forged claim arrived can be refused as stale; the next turn end
replaces the forged snapshot.

Bounds the daemon re-checks for stored items are public constants in
`pohunek-worker-protocol`: `BACKGROUND_TASK_ID_MAX_BYTES` (256),
`BACKGROUND_TASK_TYPE_MAX_BYTES` (64), `BACKGROUND_TASK_STATUS_MAX_BYTES` (32),
and `BACKGROUND_AGENT_TYPE_MAX_BYTES` (128). The worker refuses a snapshot whose
task id, type or status exceeds its bound, and drops an agent type that exceeds
its bound from that item alone.

A client of an older build decodes a kind or status that a newer daemon adds as
`other`, and the session's activity, state source, working-directory source and
a subagent's lifecycle as `unknown`; the daemon's own store and params stay
strict.

Clients read the snapshot as `SessionInfo.background`: the decimal-string
`revision`, `observed_at_ms` and the items, with `id`, `kind`, `status`,
`agent_type`, `label`, `program`, `first_seen_ms` and `last_seen_ms`. The daemon
imports it from the worker's inspect poll, not from a worker event, and only
under a hook schema that declares a subagent sequence rule, the gate the
subagent rows use. A snapshot whose items exceed the worker's bounds is
rejected. When the imported snapshot differs from the held one by value (the
revision included), the daemon stores it with the session record, publishes
`session_updated`, and then one `background_state` event. The event carries the
complete new snapshot, or `background: null` when it was cleared, and the
`runtime` (worker instance id and generation) that holds it. Clients accept an
event only when that runtime matches the current session snapshot, the rule
`subagent_state` follows; a `session.list` or `session.inspect` snapshot seeds
the state after a reconnect. A subagent item shares its `id` with its
`subagents` row, and the snapshot never changes that row.

The daemon clears the snapshot wherever it ends or loses a runtime: an exit or a
stop, a worker that is lost, reconnecting, in conflict, or incompatible. Each of
those paths publishes `background_state` with `null` after its session event,
naming the runtime that held the snapshot, when it held one. Startup reconciliation runs before
the API is exposed, so it clears without an event; a live worker found there
re-seeds the snapshot from its own inspect, and a dead or unreachable one leaves
none. A new runtime, including a recovered or restarted session, starts with no
snapshot until its own worker reports one. A worker that becomes reachable again
after an unreachable spell has its snapshot applied again, so the snapshot it
still holds is shown again.

Protocol 3 connections never receive `background_state`, and their session
objects carry no `background` key. The field and the event are current-version
additions; see the protocol window in `docs/public-api.md`.

Human-readable `pohunek session list` shows the running/recent child count in
`SUBAGENTS` and the background snapshot's per-kind counts in `BACKGROUND` (for
example `2a 2s 1m`: agents, shells, monitors; `w` workflows, `t` teammates, `o`
every other kind). `pohunek session inspect` includes the corresponding
lifecycle rows, each subagent with the label of the snapshot item of the same
id and its age, then every other background item with its kind, status,
program, label and the age since pohunek first saw it (`>=`, because Claude
reports no start time). `--json` carries the full `background` model. Streaming
clients accept `subagent_state` only when its runtime id and generation match
the current session snapshot; a full list or inspect snapshot seeds state after
reconnect or native recovery.

Foreground reconciliation selects the recognized process-group leader first;
when the leader is an unidentified wrapper, it selects a recognized member of
the same foreground PGID. It never selects a nested agent by kind from another
process group. A scan that proves a hook-reported reporter (it runs, keeps
the claim's start identity, is not owned by another session, and descends from
an observed same-runtime agent or an agent launch root without another runtime
in between) binds the claim to that process generation. A bound claim ends only
with the exit of its generation, reported by an exit watch or, when no watch can
be armed, read by every scan; failed collections, failed probes, a reporter
missing from a scan, the exit of other agents and refreshes of the same report
(also one that names no process) neither end nor renew it. When the foreground
selects an observed agent, a bound claim is kept only if that agent is one of
its ancestors at the time it was bound; a switch to any other agent replaces
it, and a foreground back on a shell root clears a claim anchored to an observed
wrapper. A claim whose probe is inconclusive (a failed liveness read, a failed
collection or an ancestry the scan cannot complete) stays up to the claim TTL
before the replacement applies. A scan neither binds nor refutes a claim admitted after it began (a hook
report, a journal projection or an adoption): the rescan the admission requests
decides, and a claim projected from the worker's journal requests that rescan.
Replacing an active agent clears stale native identity metadata
and switches the detector configuration with the new agent. An active agent
that a managed hook reports through the session's worker switches it the same
way once the daemon projects the worker's journal, and its release restores the
session's default configuration.

The durable session record keeps the active agent the worker journaled, with
whether it was reported or observed, so a daemon restart restores it even after
the worker's claim expired. A bound `session.report_agent` or
`session.release_agent` (it carries `pid` and `monotonic_sequence`) is
forwarded into the worker of a live session that negotiated worker protocol 8:
the worker validates the subject like a hook claim, journals it, and only then
answers, and the daemon commits the claim the worker holds. A bound report
therefore survives a restart whenever it is accepted, and `recorded: false`
also covers a report the worker refused. A report that is unbound (an SDK or
remote client, an older hook), that meets a protocol 7 worker, or whose forward
failed is held in memory and logged once per session and cause as
`session.active_identity.not_durable` (`unbound`, `worker_protocol_v7`,
`forward_failed`; a failed forward also logs `forward_failed` at WARN; an
accepted forward whose snapshot was not read or applied in time logs
`session.active_identity.commit_deferred` at WARN, and the next sync commits
it). It
reaches the record only with the session's next record write for another
reason, and procwatch observes the agent again after a restart, as observed.
An agent observed by procwatch is held in memory in the same way. A per-session
commit guard and the newest committed worker identity revision order a worker
snapshot against a forwarded commit: a snapshot read before the commit carries
an older revision and cannot replace the forwarded identity or its release
(#1101). A worker that predates the revision (protocol 7) gives no such order.
A record written before
schema 10 carries no origin, and its agent counts as observed when its name
equals the agent's own runtime: a reported agent under a profile named like
its runtime, nested or not, is then restored as observed until it reports
again.

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

A stop whose PTY output cannot drain still ends the session. When a process
outside the session's process group (for example a `setsid` helper) keeps the
terminal open past the stop deadline, the worker force-closes the output,
reaps the root, and records the root's exit. The session reaches `stopped`
with that exit and its session info has `output_force_closed` set to `true`,
because output the helper wrote after the close is not retained. Before the
session ends, the worker terminates every process still carrying the runtime's
`POHUNEK_WORKER_INSTANCE_ID` marker and waits until none is left (`SIGTERM`,
then `SIGKILL` after the stop grace), so a helper cannot keep writing once the
session is resumable or its worktree is released. A same-user process whose
environment cannot be read (a non-dumpable or setgid program, or one caught
between images) and that started after the worker could belong to the runtime:
the worker re-reads the process table for up to the stop grace, and if such a
process is still there it is never signalled and the cleanup stays unconfirmed.
On Linux, a zero-byte environment read is not enough to establish that a
process is unmarked: the process may be inside `execve`. The reader checks the
published environment range, accepts an empty environment only when both bounds
are equal and nonzero, and retries a contradictory nonempty range a bounded
number of times. Unpublished or persistently contradictory reads remain
unobservable. Kernel threads, identified by the kernel's `PF_KTHREAD` flag,
cannot carry runtime markers and remain unmarked even with zero bounds.
Linux has no fork-lineage proof, so a long-lived foreign process
of that kind also keeps the cleanup open. A process that scrubs its environment carries no
marker and cannot be found. If the root's exit cannot be observed, nothing is
invented and the stop fails; if the runtime's processes cannot be proven gone,
the runtime is faulted instead of ended, the stop fails with
`worker_operation_failed`, and the daemon handles it as a lost runtime
(`runtime_lost_cleanup_unconfirmed` when its own sweep cannot confirm either).
The worker log names each process that kept the cleanup open.

The same applies when the root exits on its own and such a helper keeps the
terminal open past the drain window: the worker force-closes the output, sweeps
the runtime's marked processes, and the session ends `done` or `failed` with
the root's exit and `output_force_closed` set, or its runtime becomes `lost`
when the cleanup cannot be confirmed. Either way, a helper that carries the
runtime's marker does not outlive the runtime.

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
`incompatible` means the worker is alive but the daemon does not manage it, so
it leaves the worker running: `worker_protocol_incompatible` when the worker
has no compatible private protocol version, `worker_marker_legacy` when its
journal names a worker release before 0.33.1 (or none), whose children carry
no `POHUNEK_WORKER_INSTANCE_ID` marker any sweep could attribute. Attach, input, and resize are not
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
cleanup unconfirmed: an orphan that re-executed after its creator died, and one
whose creator exited after the worker started. A process created after the worker
by launchd is decided by coalition instead: the kernel puts every process
created from the worker, through any number of forks, execs, `setsid` calls and
reparentings, in the worker's resource and jetsam coalitions, and an
unprivileged process cannot move into another job's coalition. The worker
journals its own pair as the optional `worker_coalitions` (`<resource>:<jetsam>`
decimal, no schema bump, used only in the boot it was recorded in). A hidden
process whose lineage is open and whose both coalitions differ from the
worker's is ignored and not reported; equal or unreadable coalitions prove
nothing and keep it a candidate, and a lineage descendant whose coalitions
differ is skipped, never selected. Those processes are not reported at once: while only such
unreadable processes remain, the sweep re-reads the process table for up to the
sweep grace (`sweep.grace_ms`), so one that is caught between images or about to
exit does not leave the cleanup unconfirmed; a candidate that outlives the grace
is listed. That wait is shared by every runtime one operation sweeps: the startup
reconciliation (which runs before the daemon reports readiness), one background
re-check of a session, and one removal or stop cleanup each wait at most one
sweep grace in total, however many runtimes a persistent candidate blocks; once
it has passed, a further runtime left with only such candidates is decided
without waiting and stays unconfirmed. The daemon uses the journaled number only when the journal was
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
still be needed for a re-executed orphan or a process whose creator exited after
the worker started, which the refusal lists by pid and command name for you to
judge, and for every hidden process when the worker journaled neither a creation
number nor coalitions (`worker_coalitions`). The flag is per call: nothing
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

## Temporary directory

Each session's agent runs with `TMPDIR` set to the session's own directory,
`<cache_dir>/session-tmp/<store key>/<session id>` below Pohunek's cache
directory (`$XDG_CACHE_HOME/pohunek` or the platform cache path), so agents
running side by side never overwrite each other's temporary files. The store
key comes from the metadata store's path, so two daemons with different stores
never share a directory. The directory is private
(mode 0700) and replaces a `TMPDIR` forwarded from the daemon's environment. A
profile that sets `TMPDIR` in its `[env]` keeps that value instead. Stopping a
session keeps the directory and resuming it reuses it; removing the session,
explicitly or through retention, deletes it with its content. When the daemon
starts, it deletes the directories of its own store that nothing it knows at
startup names, which a crash or a failed delete can leave behind. A directory
is kept when a session record, a resume binding, a store line the daemon
rejected that names the session, a worker or its claimed session, a
service-manager job or a worker journal names it. When it cannot tell, because
worker discovery, the job listing or the journal scan failed, or a rejected
store line names no session, it deletes nothing. A launch whose directory is not a private directory of the daemon's user fails
with `session_tmpdir_unsafe` before the agent starts.

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
transcript trees runtime descriptors declare in `[transcripts]` (Claude
`projects`, Codex `sessions`) below the config homes the host launches those
agents with (each runtime's own home and the home of every host profile,
re-read on every pass), never their parents or `$HOME`, and reads each with the
parser the descriptor names. A runtime without the declaration contributes no
root, and a runtime that is not installed contributes nothing: a host without
the official Codex or Claude Code package observes no transcript tree of that
agent and does not recognize a `codex` or `claude` process outside pohunek or
in the foreground of a shell session; a `hermes` process is likewise
recognized only through the Hermes package's detection manifest. Trees of two different runtimes that are the
same directory or nested in each other are attributed to neither; the daemon
logs the warning `external.transcript_roots.ambiguous` with code
`transcripts_ambiguous_owner`.
The transcript index is converged by a bounded reconciliation pass that runs
every 30 seconds, on a lost-event or directory-change hint, and after a
watcher restart, whether or not the live watcher (inotify on Linux, FSEvents on
macOS) is healthy. Each pass re-resolves the configured roots (symlinked or
aliased roots are scanned once, a nested root of the same runtime owns its own
transcripts), registers
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
path admits only the session's PTY root itself, attested by the kernel peer of
its local connection, and refuses every report for an `identity-v1` session
(`native_report_unattested`); a hook, a descendant of the agent, is therefore
refused there. Stale runtime,
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

Hermes, served only by its official package, uses the same durable worker
model, but only for the local interactive terminal backend in the releases its
package supports (`[0.20.0, 1.0.0)`, verified on 0.20.0). Pohunek launches it as
`hermes chat`; when a valid native reference is already present, recovery is
exactly `hermes chat --resume <reference>`. It never continues ambient Hermes
state and never reads `state.db`. Before either launch, and before material
side effects, the daemon fails with `agent_binary_missing` when the `hermes`
program does not resolve to an executable, then requires an isolated, bounded
version probe to confirm a release in the package's supported range and fails with
`agent_runtime_unsupported` when the runtime is incompatible; the refusal names
the parsed release and the supported range, never the probe output. The Hermes operator plugin reports a native
reference through its bounded lifecycle hooks for a managed Hermes session; it
does not infer or read one from Hermes state. Its resume capability is
independent from a temporarily unavailable report; fork is always unsupported
and rejects before any child/worktree side effect. See
[Hermes operator](../guides/hermes-operator.md) for the typed tool and hook
boundaries.

Where the native reference comes from is a per-runtime strategy declared in the
runtime definition's `[native_reference]` table: `hook` (a validated integration
report, the behavior of the official Codex, Claude Code and Hermes
packages), `assigned` (core
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
cannot resume its parent's conversation. A runtime whose `assigned` strategy
declares `assign_fork = true` (the official Pi package) instead launches each
fork under a reference core generates for it (`--fork <source> --session-id
<fork>` for Pi), so the fork holds its own `assigned` reference from creation
and resumes into its own conversation after a stop or a daemon restart. A later validated report always supersedes an
assigned reference: an assigned runtime that declares an integration handler and
hook schema may report, and the report replaces the stored value and its
provenance (`reported`) in the session, its durable record and its resume
binding, so `/clear` and in-session resume are followed and `session.resume` and
`session.fork` launch with the reported conversation. The declared existence
check runs for a still-assigned reference. A hook runtime may declare the same
kind of check under `[native_reference.existence]` for its id-kind reference;
the official `claude` package declares one, so its recovery additionally requires a
regular transcript file under its effective declared config home; either
recovery action refuses with `agent_native_reference_missing` before
launch when the file is absent or cannot be verified. A check may also declare
how the agent moves its store: a variable that names the store directly (the
check follows it; Pi's `PI_CODING_AGENT_SESSION_DIR`), and launch arguments or
settings-file keys it cannot follow (Pi's `--session-dir` and `sessionDir`).
`session.new`, `session.resume` and `session.fork` refuse a launch that moves
the store out of reach with `agent_native_store_relocated` before anything is
spawned; its hint names the variable to set instead. A tested worker journal
that carries a newer conversation switch the daemon cannot verify makes both
recovery actions refuse with `native_identity_uncertain` instead of resuming
the older verified target. A report must pass the usual checks (the
process-identity, sequence and expiry checks for `session.report_native_id`, the
launch process and sequence for the worker's active identity), a stale or
refused one leaves the stored reference alone, and an assigned value never
replaces a reported one, also across a daemon restart. Sequences are compared per
transport (worker claim or public report) and per runtime generation, so one
clock never makes the other stale. Inside the worker, identity claims and
releases are ordered per reported process (pid and kernel start identity): a
report must be newer than every claim and release accepted for that same
process, and reports of different processes are never compared, so a nested
agent's claim, even at `u64::MAX`, never makes the launch agent's later claim
stale. Across processes the latest accepted claim is the active identity. A
release answered `ok: false` (its process is not active) still orders that
process's later claims. The worker keeps every live reporter's sequence, never
evicting it, so a released process cannot become active again through its own
older claim delivered late. Marks of exited processes, unreaped ones
included, are pruned when room is needed; when live reporters other than the
PTY root and the launch process fill the bound (16), a further process's report
is refused with reason `identity_reporters_exhausted` until one exits. Before
the launch identity exists, a launch agent's report with a native reference is
still verified as a launch claim at the bound and kept only when it
establishes the launch identity; a deferred verification keeps only its
pending launch claim, so the retry can still establish it. Each
hook event runs a fresh interpreter for the same agent process, so the shipped
Codex and Claude hooks stamp their worker claims from the system-wide
monotonic clock in nanoseconds (`CLOCK_MONOTONIC`). A recovered
generation's first claim supersedes the reference it was relaunched with. The worker journals the latest
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
as the launch claim only from the launch process the session's hook schema
declares. Under `NamedForProvider` (`identity-subagent-v1`; it is also the core
default for a session without a schema, which is never reached because such a
session admits no hook report) that is the process named for the provider that
started first below the PTY root, never an interpreter such as `node`. Under
`FrozenRootEntrypoint` (`identity-v1`, used by Hermes, whose `hermes` entrypoint
is a Python script) it is the PTY root alone, reporting in process: the worker
requires the kernel peer of the report to be the root's PID and start time, so
a report naming the root from any other process, a descendant included, is
refused, and it accepts the root's report only while the root still runs the
exact entrypoint and shebang interpreter the worker froze before spawning it.
Every later conversation id of the root is verified the same way before it is
journaled. On the worker path only this rule binds the root's report to the
root's own kernel peer. Under `NamedForProvider` a hook reports as a descendant of the launch
process, and a descendant's report naming the launch process counts as that
process's report; every process in the agent's PTY subtree runs as the owner,
so this rule attributes reports and does not authenticate them. Under every
schema the worker refuses a report from a process that is neither the process
it names nor a descendant of it (`peer_outside_subject`). The rule requires
the entrypoint's shebang interpreter to run the script itself. An interpreter that re-executes another image, such as a macOS
framework Python stub or an `xcrun` shim like `/usr/bin/python3`, leaves the
session without a launch identity: every claim is refused as
`image_mismatch` or `cmdline_mismatch` (a `worker.launch_identity.rejected`
warning in the worker log). The runtime still runs, but native resume is
unavailable for that session. Under `NamedForProvider`, a process named for the
provider that the launch process started directly is accepted as well only in the provider's hook-helper role, which is
a command line whose second argument is `app-server` for Codex (it runs its
hooks from a `codex app-server` child of the launched `codex`); a provider
without a declared helper role gets no exemption. An independent same-provider
child, a deeper process, a sibling, a process of another name, and a process
whose PID or start time no longer matches are refused, so an agent a tool command starts never speaks for the session. The
first accepted claim is the initial reference. A later switch replaces it only
when the launch agent reports a newer verified conversation. Only the
`SessionStart` identity report carries a conversation
id; a subagent's `SubagentStart` and `SubagentStop` reports and Claude's
main-`Stop` subagent snapshot are separate claim types that never touch the
reference. The daemon's public `session.report_native_id` fallback is attested
against the kernel peer of its local connection: only the session's PTY root
itself is admitted, so a descendant of the root reporting the root's pid and
start identity is refused with `native_report_unattested` and records nothing.
A session on the `identity-v1` schema refuses that method for every report, the
root included, so its native reference comes only from the worker. Its runtime,
process-identity, ordering and expiry checks still apply to an admitted report.
The official Pi package is the worked example: see the [Pi runtime package](../guides/pi-package.md).

`session.fork` creates a new pohunek session id and PTY from the source session's
native agent conversation. The source may still be live; fork does not require a
terminal state. With `cwd_mode: "same"`, the new session starts in the source
cwd/worktree. The child begins without its own native recovery reference;
only its own verified launch-agent report can set one, except for a runtime
whose assigned reference covers forks (`assign_fork`), where the child starts
with the reference core assigned to it. The fork argv comes
from the session's frozen native-session
launch spec: Claude forks as `claude --resume <native_session_id>
--fork-session`, and a host profile that declares `fork_args` forks with exactly
those arguments. Codex fork is intentionally not enabled in this daemon contract;
Codex-backed sessions return the typed `agent_fork_unsupported` error instead
of fabricating an unsupported branch. Hermes-backed sessions return the same
typed unsupported error.

Recovery records written by earlier releases are completed when the daemon
starts, so every such session stays listed with its recovery record. The
metadata store migration maps the flat `resume_mode`/`ref_kind`/`resumable`/`fork_*`
fields of v0.33.0 onto the session's native-session launch spec with the argv
they already produced. A v0.33.1 record that kept its native reference but lost
its launch spec is restored with the spec v0.33.1 compiled in for Claude, Codex
and Hermes, which the migration keeps frozen and never reads from a current
descriptor (a restored Claude record carries its fork arguments again); for a
profile or package runtime the spec is resolved by agent name when the record
is loaded. The migration never launches anything: a restored session stays
`lost` until an explicit `session.resume`. A record of the built-in Codex,
Claude Code or Hermes, or of a host profile over one of them, is restored and
listed, but this daemon compiles none of them in: its resume and fork answer
`runtime_builtin_removed` while no package serves the alias. Once the official
package serves it, the package adopts the record (see
[official aliases](../guides/runtime-packages.md#official-aliases)) and resume
and fork run through it when the recovery contract is honored; otherwise they
answer `runtime_served_by_package`, and the migration is a new session. A record that
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
