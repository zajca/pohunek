---
type: Concept
id: concept/sessions
title: Sessions
description: Pohunek sessions are daemon-owned PTY processes controlled by the CLI and addressed locally or through a host-qualified target.
source_kind: manual
intents: [debug, help, project]
---

# Sessions

A session is a daemon-owned PTY process running an agent in a working directory.
The CLI controls sessions through the daemon: start with `pohunek session new`,
inspect with `pohunek session inspect`, list with `pohunek session list`, send
input with `pohunek session input`, stop with `pohunek session stop`, and attach
with `pohunek attach`.

Session targets are host-aware. A bare session id targets the local host; a
`<host>/<session-id>` target names a specific host. Remote session creation keeps
the existing confirmation behavior: non-local starts require explicit approval,
and JSON/non-interactive remote starts require `--yes`.

A session can carry an optional owner-set display name. Set it at creation with
`pohunek session new --name <NAME>`, and change or clear it later with
`pohunek session rename <target> <NAME>` (or `--clear`). The name is cosmetic:
it shows in `pohunek session list`, `session inspect`, and the GUI, but never
affects targeting or resume — a session is still addressed by its id. The daemon
trims the name and rejects a control character or an over-long one. The name is
captured in the resume binding, so it survives a daemon restart.

The assistant feature reuses this session lifecycle. Its opening prompt is just
initial input to a normal session, so session warnings and applied-input status
remain the source of truth for whether the agent received that prompt.

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

Attention notifications self-resolve. When the daemon observes a session's
activity return to `working`, the projector acknowledges any `unread` or `read`
`agent_blocked` and `approval_required` records for that session's
`attention:<session_id>` key, covering both hook- and projector-produced
records. This keeps a transient waiting-for-input signal from lingering as
unread after the agent has resumed; other kinds such as `error` and
`session_finished` are never auto-resolved and wait for explicit owner action.

Attention notifications are also debounced before they ever become visible. An
`agent_blocked` or `approval_required` create carrying an
`attention:<session_id>` dedupe key is held pending in memory by the daemon for
the policy's `attention_debounce_secs` window (5 seconds by default) instead of
being persisted immediately; `notification.create` still reports `created:
true` with a minted id, but the record does not appear in `notification.list`
and no `notification_created` event fires while it is pending. If the session's
activity returns to `working` inside that window, the pending record is
dropped entirely and nothing is ever created — the same self-resolve edge
described above, applied before the record surfaces rather than after. Only a
genuinely outstanding attention state, still unresolved once the window
elapses, is committed and broadcast. This is distinct from
`attention_dedupe_window_secs`, which merges duplicate reports of the same
attention moment across producers rather than delaying when it surfaces.

The GUI's Inbox modal opens a notification's message detail when it is
selected from the list, auto-marking it read. If the record links to a session
still known on the same host, the detail offers a primary Open session action
that closes the modal and selects that session; if the linked session is gone,
explanatory text replaces the button so the record is not a dead end.

Every session has an immutable launch identity: `agent` is the selected profile
name and `agent_base` is the base kind (`shell`, `codex`, or `claude`). A shell
session can temporarily host a nested Codex or Claude Code process. When that
nested agent's hook reports back, the daemon records active runtime metadata in
`active_agent`, `active_agent_base`, and the optional active native session
fields. This affects display, filtering, and detector behavior, but it does not
change the launch `agent` / `agent_base`.

Detach and client restarts do not stop a session because the daemon owns the PTY.
A daemon restart is different: the live PTY and process are gone, and only
sessions with captured native agent resume metadata can be relaunched. When an
attached terminal sees that unexpected stream close, `pohunek attach` waits for
the restarted daemon to resume the same session id and reconnects if it becomes
running again. Native resume metadata is accepted only from the session's own
agent profile or base kind, so a nested different agent cannot overwrite the
parent session's resume binding. Nested active-agent reports are therefore
runtime evidence only: they can expose the currently active agent and active
native metadata while the nested process runs, but they never populate or replace
`native_session_id` / `native_session_path` for the parent session. Releasing the
active report clears those active fields and restores the parent session's
default detector identity.

An in-memory terminal session (`stopped`, `done`, or `failed`) that still carries
captured native resume metadata can be explicitly relaunched with `session.resume`.
The daemon reuses the same pohunek session id and rebuilds the agent's native
resume argv from the frozen launch profile. The GUI's "Open in terminal" action
uses this before attaching, so a finished resumable session opens as a live PTY
instead of flashing a terminal that immediately exits. A removed session is gone
and cannot be resumed.

For project-aware work, prefer a registered project or repository target over an
ad hoc directory. See [projects](projects.md) and [worktrees](worktrees.md).
