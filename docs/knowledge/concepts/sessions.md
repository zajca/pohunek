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

A session created by dispatching a GUI review (Track D.6, see
[GUI setup](../guides/gui.md#review)) carries `review.source` (the dispatched
review's app-local id) and `review.dispatched_at` (RFC3339), plus every
`link.*` key already present on the source session, copied verbatim. Review
dispatch always targets the source session's own worktree with `cwd` rather
than minting a new one — git refuses a second worktree on a branch already
checked out — so a review session's `worktree_path` and `branch` match its
source session's.

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

Session notifications self-resolve. When the daemon observes a session's
activity return to `working`, the projector acknowledges any `unread` or `read`
`agent_blocked` and `approval_required` records for that session's
`attention:<session_id>` key and any `turn_completed` records for
`turn:<session_id>`. This keeps transient waiting-for-input and completed-turn
signals from lingering as unread after the agent has resumed; other kinds such
as `error` and `session_finished` are never auto-resolved and wait for explicit
owner action.

Session notifications are also debounced before they ever become visible. An
`agent_blocked`, `approval_required`, or session-scoped `turn_completed` create
carrying `attention:<session_id>` or `turn:<session_id>` is held pending in
memory by the daemon for the policy's `attention_debounce_secs` window (5
seconds by default) instead of being persisted immediately; `notification.create`
still reports `created: true` with a minted id, but the record does not appear
in `notification.list` and no `notification_created` event fires while it is
pending. If the session's activity returns to `working` inside that window, the
pending record is dropped entirely and nothing is ever created — the same
self-resolve edge described above, applied before the record surfaces rather
than after. Only a genuinely outstanding session signal, still unresolved once
the window elapses, is committed and broadcast. This is distinct from
`attention_dedupe_window_secs`, which merges duplicate attention reports across
producers rather than delaying when a session notification surfaces.

Unread `turn_completed` rows are bounded per session. A newer
`turn:<session_id>` record acknowledges any older unread turn for that key with
`superseded_by` pointing at the newer record, and a visible attention record for
the same session supersedes the unread turn twin because waiting-for-owner
attention includes the fact that the turn completed.

The GUI's Inbox modal opens a notification's message detail when it is
selected from the list, auto-marking it read. If the record links to a session
still known on the same host, the detail offers a primary Open session action
that closes the modal and selects that session; if the linked session is gone,
explanatory text replaces the button so the record is not a dead end.

Every session has an immutable launch identity: `agent` is the selected profile
name and `agent_base` is the base kind (`shell`, `codex`, or `claude`). A shell
session can temporarily host a nested Codex or Claude Code process. The daemon
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

`lost` means the worker or host runtime is gone and the PTY cannot be
reattached. `conflict` means discovery found ambiguous or mismatched live
identity; Pohunek quarantines it and does not kill a worker automatically.
`incompatible` means the worker is alive but has no compatible private protocol
version, so the daemon leaves it running. Attach, input, and resize are not
available in these degraded states, but list and inspect retain the logical
record and diagnostic `loss_reason`.

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
child exit, or invoke native resume. Retained raw output and terminal repaint
data are replayed as bounded contiguous worker frames, so the configured history
capacity can exceed one frame without making the live session unattachable.
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

`session.fork` creates a new pohunek session id and PTY from the source session's
native agent conversation. The source may still be live; fork does not require a
terminal state. With `cwd_mode: "same"`, the new session starts in the source
cwd/worktree and carries the same launch-agent native metadata, so the fork is
resumable too. Claude forks as `claude --resume <native_session_id>
--fork-session`. Codex fork is intentionally not enabled in this daemon contract;
Codex-backed sessions return the typed `agent_fork_unsupported` error instead
of fabricating an unsupported branch.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).
