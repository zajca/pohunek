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
`pohunek session read <target> --source recent --lines 100 --json` for the newest
bounded current-screen text. Current workers safely report `source_used:
"visible"` for recent, unwrapped, and detection requests because they do not
expose scrollback or soft-wrap metadata; `alternate_screen` remains truthful.
`pohunek session output <target> --json` for a bounded newest retained tail,
and `pohunek session wait <target> ... --timeout-ms <1..8000> --json` for one
bounded state/activity/terminal/output change. Continue output with the exact
`runtime_id`, decimal-string `runtime_generation`, and `next_offset` returned by
the previous result. A retained-history `gap` means older requested bytes were
evicted; a runtime change means discard old cursors and restart from a fresh
screen or tail. Waiting calls use dedicated connections, and their timeout is
the guaranteed waiter-slot release bound after a client disappears.

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

A session can carry an optional owner-set display name. Set it at creation with
`pohunek session new --name <NAME>`, and change or clear it later with
`pohunek session rename <target> <NAME>` (or `--clear`). The name is cosmetic:
it shows in `pohunek session list`, `session inspect`, and the GUI, but never
affects targeting or recovery — a session is still addressed by its id. The
daemon trims the name and rejects a control character or an over-long one. The
name is stored in the logical session record, so it survives daemon and worker
loss.

The assistant feature reuses this session lifecycle. Its opening prompt is just
initial input to a normal session, so session warnings and applied-input status
remain the source of truth for whether the agent received that prompt.

A session can also carry owner metadata, set atomically at creation with
repeatable `pohunek session new --meta key=value` flags (split on the first
`=`, so a value may itself contain `=`; a missing `=`, an empty key, or a key
repeated across separate `--meta` flags fails before any connection is
dialed). The daemon enforces size limits on the values. The `link.*` key
family (`link.provider`, `link.kind`, `link.id`, `link.url`, `link.branch`) is
the cross-surface convention for tying a session to a work item: both the GUI
and the launch scripts write exactly these five keys through the shared
`pohunek_prompt::link` implementation, so a link is byte-identical regardless
of which surface created the session. The daemon treats all metadata as
opaque owner-controlled strings.

Sessions created by older native GUI review flows may retain `review.source`
and `review.dispatched_at` metadata. Those keys remain opaque session metadata,
but the current native GUI no longer creates or manages reviews.

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

The GUI's Activity modal opens a notification's message detail when it is
selected from the chronological history, auto-marking it read. If the record links to a session
still known on the same host, the detail offers a primary Open session action
that closes the modal and selects that session; if the linked session is gone,
explanatory text replaces the button so the record is not a dead end. The main
session list derives Needs you only from live blocked state and active approval
records, never from unread informational history. Session detail presents
Current attention separately from Recent activity.

The notification policy also owns automatic retention. Informational/success,
warning, acknowledged attention, acknowledged error, and archived records have
separate TTLs. Unresolved action-required and error records have no automatic
TTL. After a sweep appends deletion events, the daemon atomically compacts the
JSONL action log once its configured action threshold is reached.

Every session has an immutable launch identity: `agent` is the selected profile
name and `agent_base` is the base kind (`shell`, `codex`, `claude`, or
`hermes`). A shell session can temporarily host a nested Codex or Claude Code
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

The same runtime model keeps `cwd` current. A session starts with its launch
directory, then procwatch reads the cwd of the focus process on each tick: the
active nested-agent PID when one is bound, otherwise the root PTY child. OSC 7
terminal output is accepted as an immediate cwd hint, but procwatch remains
authoritative and overwrites a hint that the process cwd contradicts. Each cwd
change emits `session_updated` and re-resolves project and worktree context. If
the new cwd is inside another registered active worktree, `worktree_path`,
`branch`, and project metadata move to that worktree; if it is outside every
known worktree, `worktree_path` is cleared while git `repo`/`branch` metadata is
kept when detection still finds a repository.

Every managed `SessionInfo` has a `runtime` object distinct from its agent
`state` and `activity`. Runtime state is one of `starting`, `live`,
`reconnecting`, `terminal`, `lost`, `conflict`, or `incompatible`. `worker_id`
identifies the PTY owner and `runtime_id` identifies one PTY generation. A
daemon restart preserves both ids. Explicit native recovery preserves the
logical session id but changes the worker and runtime ids.

`SessionInfo.capabilities.resume` and `.fork` are independent, frozen flags.
Clients must use them instead of guessing from the provider name. Long-lived
wire counters (`runtime_generation`, output offsets, terminal watermarks, and
hook sequences) are canonical unsigned decimal strings so JavaScript clients do
not lose precision.

`lost` means the worker or host runtime is gone and the PTY cannot be
reattached. `conflict` means discovery found ambiguous or mismatched live
identity; Pohunek quarantines it and does not kill a worker automatically.
`incompatible` means the worker is alive but has no compatible private protocol
version, so the daemon leaves it running. Attach, input, and resize are not
available in these degraded states, but list and inspect retain the logical
record and diagnostic `loss_reason`. After preserving diagnostic evidence, the
operator can remove a degraded logical record with `session rm`; this does not
stop or signal an unavailable or ambiguous worker.

External observer mode is opt-in with `POHUNEK_OBSERVE_EXTERNAL_AGENTS=1` (or
`SessionRegistryConfig.observe_external_agents = true`) and defaults off because
it watches provider transcript trees under the operator's Claude/Codex homes.
When enabled, the daemon combines same-user process facts with transcript JSONL
candidates to show agents that were started outside pohunek. These entries use
synthetic ids such as `ext-12345`, carry `external: true`, and appear in
`session.list`, `session.inspect`, and the GUI as read-only sessions. They have
no pohunek-owned PTY: attach, input, resize, stop, remove, rename, metadata, and
resume operations are rejected with `session_external_read_only`. The observer
removes the entry when the external process exits, including `kill -9` via the
pidfd-backed exit path.

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
`POHUNEK_WORKER_SOCKET_PATH`, and worker hook protocol version. Identity hooks
prefer the worker endpoint so accepted state survives daemon outage. Nested
active-agent reports remain runtime evidence only: they can expose the active
agent and active native metadata while that process runs, but never populate or
replace `native_session_id` / `native_session_path` for the parent session.
Startup reconciliation merges the worker's immutable launch identity into the
persisted session and recovery binding; it does not replace an already captured
native reference with an empty worker field.
If the owner-private worker identity claim cannot be delivered, the shipped
hook falls back to the local public daemon with the exact runtime id, PID and
kernel start identity, a monotonic sequence, and a short expiry. Stale runtime,
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

`session.fork` creates a new pohunek session id and PTY from the source session's
native agent conversation. The source may still be live; fork does not require a
terminal state. With `cwd_mode: "same"`, the new session starts in the source
cwd/worktree and carries the same launch-agent native metadata, so the fork is
resumable too. Claude forks as `claude --resume <native_session_id>
--fork-session`. Codex fork is intentionally not enabled in this daemon contract;
Codex-backed sessions return the typed `agent_fork_unsupported` error instead
of fabricating an unsupported branch. Hermes-backed sessions return the same
typed unsupported error.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).
