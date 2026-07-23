# Pohunek Public API

This document is the public API contract for the daemon control protocol and the
Rust SDK surface that speaks it.

Status: versioned public API, pre-1.0 stability. Breaking changes are allowed
before 1.0, but they must be reflected in the protocol version and this document.

Source of truth:

- Wire types and method constants: `crates/protocol`
- Rust SDK transport API: `crates/client`
- Daemon dispatch behavior: `crates/daemon/src/api`

## Compatibility Model

The current protocol version is `1` (`PROTOCOL_VERSION`).

Every control request, response, and event carries a numeric `v` field. The
daemon validates `request.v` before dispatching the method. In v1 negotiation is
strict equality: a client and daemon interoperate only when both speak the same
protocol version. A mismatch returns a typed `daemon/version_mismatch` error.

Clients should call `daemon.health` after opening a control connection to learn
the daemon build version and protocol version, but `daemon.health` is not a
special unauthenticated handshake. It is an ordinary request and is negotiated
like every other method.

Additive changes do not require a version bump:

- New optional fields may be added to params, results, errors, or events.
- Unknown fields must be ignored by receivers.
- Omitted optional fields must retain their documented default.
- New methods and new error codes are additive; older daemons return
  `daemon/method_not_found` for unknown methods.

Non-additive wire changes require a protocol version bump. Examples: changing a
required field name or type, removing a field, changing enum string values,
changing an existing method's result shape, or changing attach stream framing.

## Transports

The daemon exposes the same protocol on two transports:

| Transport | Endpoint | Security boundary |
|---|---|---|
| Local | Unix socket at the configured runtime path | Owner-only socket directory and mode |
| Remote | TCP listener bound to the host's NetBird address | NetBird/WireGuard reachability and policy |

The JSON control stream is newline-delimited UTF-8 JSON. One JSON value is sent
per line. The current daemon and Rust SDK cap control lines at 1 MiB.

Raw terminal bytes are never multiplexed onto a JSON control connection. Attach
uses a separate connection described in "Attach Stream".

The TypeScript SDK also supports a WebSocket relay transport for browser and
Bun/Node clients that cannot dial Unix sockets or daemon TCP directly. Browser
code imports the browser-safe `@pohunek/sdk/browser` entry; the root
`@pohunek/sdk` entry additionally exposes Bun/Node socket transports. The relay
is not a daemon protocol endpoint and does not aggregate state. It is a pure
one-WebSocket-to-one-daemon-connection tunnel:

- `GET /daemon/<host>/control` upgrades to a WebSocket whose text frames are
  control lines. The relay writes each frame's UTF-8 bytes plus the daemon's
  newline delimiter to one daemon control connection, and sends each daemon
  newline-delimited response/event line back as one text frame.
- `GET /daemon/<host>/attach` upgrades to a WebSocket whose binary frames are
  opaque attach bytes. The relay forwards bytes unframed to/from one raw daemon
  connection.
- The relay enforces the 1 MiB control-line cap in both directions and closes
  the WebSocket on oversize input. It does not parse JSON, multiplex sessions,
  discover hosts, or retain protocol state.
- The `<host>` URL segment is resolved only through the relay operator's static
  target map. Unknown hosts are rejected during upgrade.
- The relay must bind fail-closed to a NetBird CGNAT address
  (`100.64.0.0/10`). Loopback is allowed only for explicit local testing or
  development; wildcard addresses such as `0.0.0.0` and `::` are never valid.

This WebSocket relay framing contract is pre-1.0 transport infrastructure. It
is intentionally re-reviewable when Track B starts so the browser control
center can validate the relay boundary before any stability promise is made.

## Envelopes

### Request

```json
{"v":1,"id":"req-7f3","method":"session.list","params":{}}
```

Fields:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `v` | integer | yes | Protocol version spoken by the client. |
| `id` | string | yes | Correlation id. The response echoes it. |
| `method` | string | yes | One of the public method names below. |
| `params` | JSON value | no | Method-specific params. Missing defaults to `null`. |

For parameterless methods, send `params: null` or omit `params` unless a method
documents another defaultable object.

### Response

Successful response:

```json
{"v":1,"id":"req-7f3","ok":{"status":"ok"}}
```

Error response:

```json
{
  "v": 1,
  "id": "req-7f3",
  "err": {
    "class": "daemon",
    "code": "method_not_found",
    "msg": "unknown control method: example.missing"
  }
}
```

Exactly one of `ok` or `err` is present.

### Event

Events are pushed only after a successful `subscribe` request. They are also
newline-delimited JSON, one event per line:

```json
{"v":1,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title"}
```

Event payload fields are flattened at the top level beside `v`, `event`, and the
optional `id`.

## Public Methods

All params and result type names below refer to structs exported by
`crates/protocol`. JSON field names are the Serde field names of those structs.

| Method | Params | `ok` result | Notes |
|---|---|---|---|
| `daemon.health` | `null` | `{status, daemon_version, protocol_version}` | Liveness and version probe. |
| `daemon.doctor` | `null` | `DaemonDoctorResult` | Runs daemon-local checks. Non-null params are `daemon/bad_request`. |
| `host.inspect` | `null` | `HostCapabilities` | Live capability snapshot for the daemon's host. |
| `host.discover` | `HostDiscoverParams` or `null` | `Vec<HostRecord>` | Enumerates NetBird peers and classifies daemon reachability. |
| `session.new` | `SessionNewParams` | `SessionNewResult` | Starts an agent PTY session. Optional `metadata` is written atomically with the session (see the `metadata` field note under `SessionInfo` below); the CLI exposes it as repeatable `--meta key=value`. |
| `session.list` | `SessionListParams` or `null` | `Vec<SessionInfo>` | Lists sessions; filters use AND semantics. |
| `session.inspect` | `SessionId` | `SessionInfo` | `SessionId` is a JSON string, e.g. `"s-1"`. |
| `session.stop` | `SessionId` | `SessionStopResult` | Stops a live session (the entry stays in `list`). |
| `session.resume` | `SessionId` | `SessionResumeResult` | Relaunches a terminal session from captured native resume metadata, reusing the same session id. Live sessions return `session_not_terminal`; terminal sessions without native metadata return `not_resumable` or `agent_not_resumable`. |
| `session.fork` | `SessionForkParams` | `SessionForkResult` | Forks a native agent conversation into a new pohunek session id and PTY, using the source session's cwd/worktree for `cwd_mode: "same"`. Live sources are allowed. Unknown ids return `session_not_found`; external sessions return `session_external_read_only`; sources without launch-agent native metadata return `not_resumable` or `agent_not_resumable`; Codex-backed sessions return `agent_fork_unsupported`. A successful fork emits `session_created`. |
| `session.remove` | `SessionId` | `SessionRemoveResult` | Evicts a session from the registry, stopping it first if still live. Unknown id is `session_not_found`. |
| `session.attach` | `SessionAttachParams` | `SessionAttachResult` | Mints a one-shot attach stream id. |
| `session.detach` | `SessionDetachParams` | `SessionDetachResult` | Cancels an active attach stream. Unknown streams return `detached: false`. |
| `session.resize` | `SessionResizeParams` | `SessionResizeResult` | Resizes the PTY on the control connection. |
| `session.input` | `SessionInputParams` | `SessionInputResult` | Injects text using agent-specific input framing. |
| `session.report_agent` | `SessionReportAgentParams` | `SessionReportAgentResult` | Hook callback for nested agents running inside an existing session. It records an active-agent claim, optional process binding, and optional active native metadata without changing launch identity or resume binding; ignored reports return `recorded: false`. Claims are reconciled with process facts and can be auto-released when no live backing process remains. |
| `session.release_agent` | `SessionReleaseAgentParams` | `SessionReleaseAgentResult` | Hook callback that clears a matching active nested-agent report and restores the session's default detector identity. Claude `SessionEnd` hooks use this as the clean-exit fast path; non-current releases return `released: false`; process-backed auto-release uses the same clear path. |
| `session.report_native_id` | `SessionReportNativeIdParams` | `SessionReportNativeIdResult` | Hook callback for launch-agent resume metadata. The daemon records only reports whose `agent` matches the session profile name or base kind; ignored reports return `recorded: false`. This is not the nested-agent active identity callback. |
| `session.set_metadata` | `SessionSetMetadataParams` | `SessionSetMetadataResult` | Merges owner-controlled metadata. Values must not contain secrets. |
| `session.rename` | `SessionRenameParams` | `SessionRenameResult` | Sets or clears a session's owner display name (`name: null` clears). Cosmetic; the daemon trims it and rejects a control character or over-long name. |
| `session.diff` | `SessionDiffParams` | `SessionDiffResult` | Computes a unified diff of a session's worktree against a base ref. `base: null` defers to the worktree binding's recorded base branch, then the repository default. A session without a bound worktree returns `session_no_worktree`; a hostile explicit `base` (empty, leading `-`, or a control character) returns `invalid_branch`; a `base` that cannot be resolved to a merge-base against `HEAD` returns `session_diff_base_unresolved`. See `SessionDiffResult` under Core Payloads for the size cap and truncation semantics. |
| `subscribe` | `null` | `{subscribed: true}` then event stream | Consumes the connection into a one-way event stream. |
| `integration.install` | `IntegrationInstallParams` or `null` | `IntegrationInstallResult` | Installs agent hooks for active-agent state, native session id capture, and provider notifications. |
| `assistant.materialize` | `AssistantMaterializeParams` | `AssistantMaterializeResult` | Materializes the assistant knowledge bundle on the daemon host. |
| `notification.create` | `NotificationCreateParams` | `NotificationCreateResult` | Creates a host-local notification. Daemon policy is enforced for every producer, including provider hooks and daemon projectors. Dedupe may return `created: false` with an existing or upgraded record. `agent_blocked`/`approval_required` with `attention:<session_id>` and `turn_completed` with `turn:<session_id>` are deferred: the result still reports `created: true` with a minted id, but the record is held pending until `attention_debounce_secs` elapses; see `NotificationPolicy`. |
| `notification.list` | `NotificationListParams` or `null` | `NotificationListResult` | Lists notification records with exact-match filters and cursor pagination. Deleted records are excluded unless `status: deleted` is requested. |
| `notification.update` | `NotificationUpdateParams` | `NotificationUpdateResult` | Updates one record's lifecycle status. Allowed transitions are `unread -> read`, `read -> acknowledged`, `unread -> acknowledged`, `unread/read/acknowledged -> archived`, and any non-deleted status to deleted. |
| `notification.delete` | `NotificationDeleteParams` | `NotificationDeleteResult` | Logically deletes one record. Unknown or already-deleted ids return `deleted: false`. |
| `notification.policy.get` | `null` | `NotificationPolicyResult` | Reads the daemon's notification policy. Non-null params are `daemon/bad_request`. |
| `notification.policy.set` | `NotificationPolicyParams` | `NotificationPolicyResult` | Replaces and persists the daemon notification policy at `<data_dir>/notifications/policy.json`. |
| `notification.retention.prune` | `NotificationRetentionParams` or `null` | `NotificationRetentionResult` | Explicitly deletes records selected by retention filters, or reports matches when `dry_run` is true. |
| `project.list` | `ProjectListParams` or `null` | `Vec<ProjectInfo>` | Lists known projects on the target host. |
| `project.add` | `ProjectAddParams` | `ProjectInfo` | Registers a host-local git project path. |
| `project.show` | `ProjectShowParams` | `ProjectShowResult` | Shows a project plus live worktree state. |
| `project.rename` | `ProjectRenameParams` | `ProjectInfo` | Sets a custom display label. |
| `project.remove` | `ProjectRemoveParams` | `ProjectRemoveResult` | Removes a project record and optionally owned worktrees. |
| `project.prompt` | `ProjectPromptParams` | `ProjectPromptResult` | Resolves a prompt template without rendering it. |
| `project.action` | `ProjectActionParams` | `ProjectActionResult` | Resolves one action recipe plus prompt content. |
| `project.actions` | `ProjectActionsParams` | `ProjectActionsResult` | Lists available project actions after layer shadowing. |
| `worktree.remove` | `WorktreeRemoveParams` | `WorktreeRemoveResult` | Removes one owned worktree binding, refusing live sessions unless forced by the method contract. |

`status` exists as a method constant in `crates/protocol` but is not a supported
daemon method in this API version. It returns `daemon/method_not_found`.

### Daemon Runtime Configuration

`POHUNEK_OBSERVE_EXTERNAL_AGENTS` is an opt-in daemon environment flag. Accepted
true values are `1`, `true`, `yes`, and `on`; accepted false values are `0`,
`false`, `no`, `off`, or an unset variable. When true, the daemon watches the
operator's Claude and Codex transcript trees and same-user process table for
agents started outside pohunek. The corresponding `SessionRegistryConfig`
setting is `observe_external_agents`, default `false`.

## Core Payloads

This section names the high-value fields clients commonly branch on. The full
wire shapes are the exported `crates/protocol` structs.

### `SessionInfo`

Important fields:

- `id`: stable session id.
- `external`: optional bool. `false` means a normal pohunek-owned PTY session;
  `true` means an opt-in observed external agent. External sessions are
  read-only: attach, input, resize, stop, remove, rename, metadata updates, and
  resume return `runtime/session_external_read_only`.
- `name`: optional owner-set display name; absent means the session is shown by
  its id. Set at `session.new` and changed via `session.rename`.
- `agent`: profile name.
- `agent_base`: `shell`, `codex`, or `claude`.
- `active_agent`: optional runtime agent profile currently active inside the
  session. Present for nested agents reported through hooks or inferred from
  process facts.
- `active_agent_base`: optional runtime base kind (`shell`, `codex`, or
  `claude`) for `active_agent`.
- `active_agent_pid`: optional process id backing `active_agent`. When present,
  the daemon auto-releases the active agent if that process exits.
- `active_agent_session_id` / `active_agent_session_path`: optional native
  metadata for the active nested agent. These fields are display/runtime
  metadata only and do not make the parent session resumable as that nested
  agent.
- `cwd`: current host-local working directory. It starts as the launch
  directory and can change while the session runs when procwatch observes the
  focus process in a new directory or the PTY emits an OSC 7 cwd hint.
- `cwd_source`: optional source of the current `cwd`: `launch`, `procwatch`, or
  `osc7`. `procwatch` is authoritative; `osc7` is an immediate hint that the
  next procwatch tick can overwrite if the focus process disagrees.
- `pid`: root process id, or the observed external agent process id.
- `cols`, `rows`: current PTY size. External sessions have no PTY and report
  `0x0`.
- `state`: `starting`, `running`, `stopped`, `done`, or `failed`.
- `state_source`: `osc_title`, `osc_progress`, `screen`, `process`, or
  `report`.
- `activity`: optional `working`, `blocked`, or `idle`.
- `native_session_id` / `native_session_path`: optional agent resume binding.
  These belong to the immutable launch agent and are written by
  `session.report_native_id`, not by nested active-agent reports. A forked
  session copies the source launch-agent native metadata so the new session is
  also resumable.
- `project_id`, `project_label`, `repo`, `branch`, `worktree_path`: optional git
  and project context for the current `cwd`. A cwd change re-resolves this
  context. When a session leaves every known active worktree, `worktree_path` is
  cleared; `repo` and `branch` remain populated when git detection still finds a
  repository at the new cwd.
- `warnings`: non-fatal worktree setup warnings.
- `metadata`: owner-controlled strings; must not contain secrets. The daemon
  treats every key opaquely; clients own the convention. One such
  client-defined convention is the `link.*` key family (`link.provider`,
  `link.kind`, `link.id`, `link.url`, `link.branch`) written by the GUI and
  the launch scripts to tie a session to a work item — no protocol surface
  is dedicated to it.
- `created_at`, `updated_at`: RFC3339 timestamps.
- `exit_code`: optional process exit code.

### Active-Agent Hook Payloads

`session.report_agent` accepts the nested agent `source`, `agent`, optional
`activity`, optional `seq`, optional `pid`, and optional active native metadata.
`pid` is the OS process id for the active nested agent. When present, the daemon
binds the active claim to that process and clears the claim when procwatch sees
the process exit. The shipped integration state hooks use
`POHUNEK_INTEGRATION_VERSION=2` and send the hook process's parent PID on
`SessionStart`.

`session.release_agent` accepts the same `source`/`agent` identity plus an
optional `seq`. A release clears only the current matching active-agent claim;
stale releases do not clear newer reports. Claude installs a `SessionEnd` state
hook that sends `session.release_agent` with a fresh timestamp sequence, so
clean exits normally clear active state promptly. Codex has no installed
session-end release path because its `Stop` hook is turn completion, not process
exit; procwatch remains the Codex release backstop.

### Session and Project Filters

`session.list` and `project.list` filters are exact-match predicates combined
with AND semantics. Session `agent` filters match the immutable launch profile
or base kind, and also match the current `active_agent` profile or base kind
when a nested agent has reported itself. They are tagged objects, for example:

```json
{
  "filters": [
    {"key":"state","value":"running"},
    {"key":"agent","value":"codex"}
  ]
}
```

With the example above, a shell session that currently has
`active_agent: "codex"` also matches `{"key":"agent","value":"codex"}` even
though its launch `agent` remains `shell`.

### `NotificationRecord`

Important fields:

- `id`: stable host-local notification id.
- `source`: sanitized producer identity with `provider`, `provider_event`, and
  `host_local_source_id`. Provider hooks use `codex` or `claude`; daemon
  projectors use `pohunek`.
- `kind`: `agent_blocked`, `approval_required`, `turn_completed`,
  `session_finished`, `error`, or `system`.
- `severity`: `info`, `success`, `warning`, `error`, or `action_required`.
- `status`: `unread`, `read`, `acknowledged`, `archived`, or `deleted`.
- `title` / `body`: bounded, sanitized user-facing text. Notification payloads
  must not contain raw terminal output, prompts, secrets, environment dumps, or
  full tool results.
- `metadata`: safe producer tags. The daemon accepts at most eight entries, with
  values at most 512 characters, and only allowlisted keys: `action_url`,
  `detail_url`, `provider`, `provider_event`, `reason`, `summary`,
  `hook_event_id`, `matcher`, and `tool_name`. Secret-shaped keys such as
  `token`, `secret`, `password`, `api_key`, `authorization`, and `cookie` are
  rejected.
- `session_id`: optional linked session id. It is shape-validated when supplied
  by `notification.create` and may point to a session that no longer exists.
- `agent_kind`, `project_id`: optional display and filtering context.
- `source_id`: optional producer-specific id used for idempotence within one
  source namespace.
- `dedupe_key`: optional source-independent id for one logical event. Session
  attention notifications use `attention:<session_id>`; session turn-completion
  notifications use `turn:<session_id>`.
- `read_at`, `acked_at`, `archived_at`, `deleted_at`: lifecycle timestamps set
  by status transitions.
- `superseded_by`: optional replacement link. Older unread `turn_completed`
  records are acknowledged with `superseded_by` pointing at the newer turn or
  attention record that made them stale.

`notification.list` sorts by `created_at` descending, then `id`, and omits
deleted records by default. `NotificationListParams` can filter by status, kind,
severity, provider, session id, and creation time range, plus `limit` and
`cursor`.

### `NotificationPolicy`

Important fields:

- `attention_dedupe_window_secs`: window for source-independent attention
  dedupe. The default is 120 seconds.
- `attention_debounce_secs`: shared window a deferred session attention or
  turn-completion notification is held pending before it is allowed to surface.
  The default is 5 seconds. Additive: a policy JSON written before this field
  existed loads the default.
- `enabled`: default per-kind flags.
- `codex` / `claude`: optional provider-specific per-kind overrides.

Default policy enables `agent_blocked`, `approval_required`, and `error`.
`turn_completed`, `session_finished`, and `system` are implemented but disabled
by default.

Policy is enforced daemon-side for all notification producers. If a producer
creates a disabled kind, `notification.create` returns
`runtime/notification_kind_disabled`.

Provider hooks have higher source priority than daemon projectors for the same
`attention:<session_id>` key inside `attention_dedupe_window_secs`. A Codex or
Claude hook can upgrade an existing projector attention record in place; the
daemon returns `created: false` and emits `notification_updated`. A later
projector create for an existing provider-backed attention record is suppressed
and returns `created: false` with the existing record. Producers other than
Codex, Claude, `pohunek`, or `daemon` are treated as user/external sources and
do not automatically supersede provider records.

`turn:<session_id>` has different semantics: it is not time-window deduped. A
new unread `turn_completed` for the same session acknowledges any older unread
turn immediately and sets the older record's `superseded_by` to the newer id.
The older record remains in history but disappears from default unread inbox
views. When an attention record for the same session becomes visible, it
acknowledges any unread `turn:<session_id>` twin with `superseded_by` pointing at
the attention record because `agent_blocked`/`approval_required` subsumes "the
turn completed and is waiting".

When a session's activity enters `working`, the daemon resolves both
`attention:<session_id>` and `turn:<session_id>`. Pending records with those
keys are dropped before they ever persist, and already-visible unread/read
matching records are acknowledged with `notification_updated`.

#### Session notification debounce

`agent_blocked`, `approval_required`, and session-scoped `turn_completed` are
held pending rather than persisted immediately when they carry
`attention:<session_id>` or `turn:<session_id>`. The daemon mints the
notification id and returns `notification.create` result `created: true` with
the full record, but the record is not written to the store and does not appear
in `notification.list` until it flushes. No `notification_created` event is
emitted for a pending record.

The daemon holds the pending record for `attention_debounce_secs`. If the
session resolves back to `working` within that window, the pending record is
dropped entirely: nothing is ever persisted, and no event fires. Only if the
window elapses with the session signal still outstanding does the daemon commit
the record through the store and emit `notification_created`, exactly as an
immediate create would. Debounce does not apply to `session_finished`, `error`,
or `system`.

`attention_dedupe_window_secs` and `attention_debounce_secs` are independent and
answer different questions:

- `attention_dedupe_window_secs` controls whether two producers reporting the
  *same* attention moment (a provider hook and the daemon projector) collapse
  into one record instead of two.
- `attention_debounce_secs` controls *whether and when* a pending session
  notification is allowed to surface at all, regardless of how many producers
  reported it.

### Provider Notification Hooks

`integration.install` installs durable state and notification hook adapters for
current Codex and Claude builds only. There is no fallback for older provider
hook APIs.

Codex notification support requires modern lifecycle hooks for
`PermissionRequest` and `Stop`. The installer writes managed command hooks to
`hooks.json` and records trust metadata in `config.toml`; the legacy Codex
`notify` key is not used and is not sufficient for approval notifications.

Claude notification support requires hook events for `Notification`, `Stop`,
and `StopFailure`. `Notification` matcher values map as follows:
`permission_prompt` and `elicitation_dialog` create `approval_required`,
`idle_prompt` creates `agent_blocked`, and `auth_success`,
`elicitation_complete`, and `elicitation_response` create `system`. `Stop`
creates `turn_completed`; `StopFailure` creates `error`.

When `POHUNEK_SESSION_ID` is valid, hook adapters add
`attention:<session_id>` to attention events and `turn:<session_id>` to
`Stop`/`turn_completed` events. Invalid session ids are dropped before either
`session_id` or `dedupe_key` reaches the daemon.

Hook adapters read at most 64 KiB from provider stdin, validate action and
environment before reading input, silently drop an invalid `POHUNEK_SESSION_ID`,
and exit successfully without output on local failures so agent sessions are not
disrupted. Reinstalling hooks removes only exact command shapes managed by
Pohunek; user hooks that merely reference the managed script path are preserved.

### `SessionDiffResult`

Important fields:

- `diff`: unified diff text of the session's worktree against `base`. Covers
  tracked changes plus untracked files (rendered as added-file diffs); binary
  files appear as git's usual "Binary files differ" stanza.
- `base`: the base ref the diff was actually computed against — the caller's
  explicit `SessionDiffParams.base` when given, otherwise the resolved
  worktree/repository default. Always present even when the request omitted
  `base`, so a client can display which ref it diffed against.
- `truncated`: `true` when `diff` was cut short at a file boundary to stay
  within `MAX_SESSION_DIFF_BYTES` (half of `MAX_CONTROL_LINE_BYTES`, chosen so
  the full response envelope always fits one control line). When `true`, later
  files in the change set are omitted from `diff` entirely; a client should
  surface this rather than treat the diff as complete.

## Error Contract

Every error body has this shape:

```json
{"class":"runtime","code":"session_not_found","msg":"session not found: s-1","recover":"optional hint"}
```

Fields:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `class` | string | yes | Broad category. |
| `code` | string | yes | Stable machine-readable code. Treat the set as open. |
| `msg` | string | yes | Human-readable, non-secret diagnostic. |
| `recover` | string | no | Optional recovery hint. |

Classes:

| Class | Meaning |
|---|---|
| `configuration` | Missing or invalid required configuration. |
| `daemon` | Daemon-level failures: bad request, unknown method, version mismatch, daemon unavailable. |
| `transport` | Framing, connection, or host reachability failures. |
| `runtime` | Session, PTY, agent, project, worktree, or assistant runtime failures. |
| `discovery` | NetBird CLI/state/host discovery failures. |

Canonical public codes currently emitted include:

| Class | Codes |
|---|---|
| `configuration` | `paths_unavailable` |
| `daemon` | `version_mismatch`, `method_not_found`, `bad_request`, `daemon_unreachable`, `remote_daemon_unavailable`, `projects_not_configured`, `serialize_failed`, `json_error`, `project_task_panicked`, `doctor_task_panicked`, `assistant_materialize_task_panicked`, `assistant_method_unsupported`, `attach_self_feedback` |
| `transport` | `framing`, `host_unreachable` |
| `discovery` | `netbird_cli_missing`, `netbird_state_unavailable`, `host_unknown`, `remote_discovery_failed` |
| `runtime` | `agent_binary_missing`, `agent_profile_not_found`, `invalid_profile`, `agent_not_resumable`, `not_resumable`, `invalid_session_ref`, `no_capable_agent`, `bundle_unavailable`, `assistant_bundle_mismatch`, `materialization_failed`, `agent_cannot_read_bundle`, `session_not_found`, `session_not_running`, `session_not_terminal`, `session_external_read_only`, `session_exit_timeout`, `attach_not_found`, `attach_expired`, `pty_alloc_failed`, `spawn_failed`, `pty_error`, `io_error`, `project_store_error`, `project_detect_failed`, `not_a_git_repo`, `project_not_found`, `project_ambiguous`, `prompt_not_found`, `template_not_found`, `action_not_found`, `invalid_name`, `invalid_template`, `invalid_action`, `path_escape`, `config_read_failed`, `agent_not_installable`, `agent_config_dir_missing`, `integration_settings_invalid`, `integration_io_failed`, `worktree_store_error`, `worktree_path_conflict`, `invalid_base_branch`, `worktree_branch_in_use`, `worktree_add_failed`, `invalid_branch`, `invalid_branch_slug`, `notifications_not_configured`, `notification_task_panicked`, `notification_store_error`, `notification_not_found`, `invalid_notification_transition`, `invalid_notification_metadata`, `invalid_notification_session_id`, `invalid_notification_dedupe_key`, `notification_kind_disabled`, `invalid_notification_timestamp`, `invalid_notification_cursor` |

Clients must not parse `msg`. Branch on `class` and `code`, then display `msg`
and `recover` for unknown codes.

## Events

`subscribe` turns the control connection into a one-way event stream after this
ack:

```json
{"v":1,"id":"sub-1","ok":{"subscribed":true}}
```

The daemon then writes these events:

| Event | Payload | Meaning |
|---|---|---|
| `session_created` | `{session: SessionInfo}` | A session was created, forked, or explicitly resumed into a new live PTY. |
| `session_updated` | `{session: SessionInfo}` | Session metadata, active-agent report/release, cwd/worktree/project association, state, resize, resume binding, or terminal state changed. |
| `session_stopped` | `{session: SessionInfo}` | A user-requested stop completed. |
| `session_removed` | `{session: SessionInfo}` | A session was evicted from the registry; clients drop it from their view. |
| `agent_state` | `{session_id: SessionId, activity: AgentActivity, source: StateSource}` | Agent activity changed. `source` may be `report` when a hook report supplied explicit active-agent state. |
| `attach_opened` | `{session_id: SessionId, stream_id: string}` | A pending attach token was redeemed and a raw stream opened. |
| `attach_closed` | `{session_id: SessionId, stream_id: string}` | A raw attach stream ended or was detached. |
| `notification_created` | `{record: NotificationRecord}` | A durable notification record was created. |
| `notification_updated` | `{record: NotificationRecord}` | A notification record changed lifecycle status, was upgraded by higher-priority source dedupe, or was acknowledged by resolve/supersede processing. |
| `notification_deleted` | `{notification_id: NotificationId}` | A notification record was logically deleted. |

Subscription connections ignore further client input after the ack. A slow
subscriber may miss older events if the daemon's internal event channel lags; it
should reconcile by calling `session.list`, `session.inspect`, or
`notification.list`.

## CLI Notification Surface

The CLI exposes durable notifications through `pohunek notifications`:

- `pohunek notifications list`: list records on one host; `--all-hosts` includes
  local plus all reachable daemon hosts discovered by the local daemon.
- `pohunek notifications watch`: stream `notification_created`,
  `notification_updated`, and `notification_deleted`; `--all-hosts` opens one
  subscription per reachable host.
- `pohunek notifications read|ack|archive|delete <target>`: update one record.
  Targets accept bare `id` or `host/id`; an explicit target host overrides
  `--host`.
- `pohunek notifications policy get|set`: read policy or toggle one
  provider/kind flag. `set` accepts `--provider default|codex|claude`,
  `--kind <kind>`, and exactly one of `--enabled` or `--disabled`.
- `pohunek notifications retention prune`: explicitly prune records selected by
  `--status`, `--before`, and `--limit`, with exactly one of `--dry-run` or
  `--apply`.

Commands that support `--all-hosts` render per-host successes and structured
per-host errors. Cross-host notification aggregation is client-side; no central
notification server is introduced.

## Attach Stream

The attach byte stream is part of protocol v1. It is not an implementation
detail of the CLI.

Sequence:

1. On a normal control connection, send `session.attach` with
   `SessionAttachParams`.
2. The daemon returns `SessionAttachResult` with a one-shot `stream_id`.
3. Open a second connection to the same daemon and transport family.
4. Send exactly one newline-delimited attach prelude:

   ```json
   {"attach":"a-1"}
   ```

5. After the prelude newline, the connection switches to raw bidirectional PTY
   bytes. Terminal output flows daemon-to-client; user input flows
   client-to-daemon.
6. Send `session.resize` and `session.detach` on the control connection, not on
   the raw byte stream.

Attach stream rules:

- The prelude has no `v` field. Its version is governed by the control protocol
  version that minted the `stream_id`.
- The prelude object must contain exactly one field, `attach`, with a non-empty
  string.
- `stream_id` is one-shot and short-lived. The current default TTL is 10 seconds.
- If redemption fails, the daemon replies with a normal error response on the
  second connection and does not switch to raw byte mode.
- After successful redemption, bytes are opaque. Clients must not assume UTF-8.
- `session.detach` cancels an active stream by `stream_id`; closing the raw
  socket also ends the attach.
- `session.attach` may include `origin_session_id` and `origin_daemon_id`. When
  both identify the same daemon-owned session the client is already running
  inside, the daemon rejects the attach with `daemon/attach_self_feedback`.
- On the WebSocket relay transport, the attach prelude is sent as the first
  bytes on the `/daemon/<host>/attach` binary WebSocket. After redemption, every
  binary frame remains opaque PTY data.

Rust SDK helpers:

- `attach_raw(host, socket_path, stream_id)`
- `attach_raw_local(socket_path, stream_id)`
- `attach_raw_tcp_addr(host, addr, stream_id)`
- `*_with_options` variants

These helpers open the raw connection and write the prelude before returning a
`RawStream`.

## Rust SDK Surface

The `pohunek-client` crate is the supported Rust client surface. New Rust
clients should use it rather than hand-writing protocol framing.

Public exports:

- `protocol`: re-export of `pohunek-protocol`.
- `Client`: framed request/response and subscription client.
- `ClientOptions`: `request_timeout` and `connect_timeout`, both defaulting to
  5 seconds.
- `Subscription`: raw event-line stream after a successful `subscribe`.
- `RawStream`: local Unix or remote TCP raw byte stream for attach.
- `ClientError`: SDK error enum with `to_protocol_error()` for structured
  rendering.
- `next_request_id(method)`: shared correlation-id generator used by SDK-backed
  clients.
- Raw and attach helpers: `connect_raw*` and `attach_raw*`.

Connection APIs:

- `Client::connect(host, socket_path)`: `""` and `"local"` use the Unix socket;
  any other host is resolved through NetBird and dialed over TCP.
- `Client::connect_local(socket_path)`: direct Unix socket.
- `Client::connect_tcp_addr(host, addr)`: direct TCP with host context preserved
  for remote errors.
- `*_with_options` variants accept `ClientOptions`.

Request APIs:

- `Client::call::<M: protocol::Method>(params) -> M::Output`: sends one typed
  method request, pairing the method name, params, and success payload through
  marker types in `protocol::method`.
- `Client::handshake() -> ProtocolVersion`: calls `daemon.health` and returns the
  daemon-reported protocol version.
- `Client::request(&Request) -> serde_json::Value`: sends one request and returns
  the raw `ok` payload for low-level callers and framing tests.
- `Client::subscribe(&Request) -> Subscription`: consumes the client connection
  after a subscribe ack.
- `Subscription::next_line() -> Option<String>`: returns raw event JSON lines.
- `Subscription::next_event() -> Option<Event>`: decodes one event JSON line into
  the protocol event envelope.
- `Client::create_notification(NotificationCreateParams)`: calls
  `notification.create`.
- `Client::list_notifications(NotificationListParams)`: calls
  `notification.list`.
- `Client::update_notification(NotificationUpdateParams)`: calls
  `notification.update`.
- `Client::delete_notification(NotificationDeleteParams)`: calls
  `notification.delete`.
- `Client::get_notification_policy()`: calls `notification.policy.get`.
- `Client::set_notification_policy(NotificationPolicyParams)`: calls
  `notification.policy.set`.
- `Client::prune_notifications(NotificationRetentionParams)`: calls
  `notification.retention.prune`.

SDK error mapping preserves daemon protocol errors and adds host/transport
context for local and remote failures. Use `ClientError::to_protocol_error()` to
render SDK failures in the same envelope taxonomy as daemon errors.

## TypeScript SDK Surface

The `@pohunek/sdk` package mirrors the Rust SDK surface for TypeScript clients.
Its browser-safe `@pohunek/sdk/browser` entry contains no `node:net` imports and
exports only the shared runtime and WebSocket path. Domain types, method maps,
event unions, constants, and generated protocol types come from
`@pohunek/protocol`; the SDK owns envelopes, framing, transports,
request/subscription orchestration, attach helpers, and structured client
errors.

Public exports:

- `Client`: framed request/response and subscription client.
- `nextRequestId(method)`: shared correlation-id generator used by SDK-backed
  TypeScript clients.
- `SocketTransport`: direct Unix/TCP `node:net` transport for Bun/Node; exported
  only by the root `@pohunek/sdk` entry.
- `WsTransport`: WebSocket relay transport using the WHATWG `WebSocket` global.
- `Transport`: pluggable transport interface with `control()` for framed
  control channels and `raw()` for unframed attach channels.
- `ControlChannel`: `send(line)`, async `lines`, and `close()` for one framed
  control connection.
- `RawDuplex`: `ReadableStream<Uint8Array>`, `WritableStream<Uint8Array>`, and
  `close()` for one raw attach connection.
- `ConnectOptions`: TypeScript counterpart to Rust `ClientOptions`; the package
  does not export a separate `ClientOptions` alias. It carries
  `connectTimeoutMs` and `requestTimeoutMs`, both defaulting to 5000 ms.
- `ResolvedConnectOptions`, `DEFAULT_CONNECT_TIMEOUT_MS`,
  `DEFAULT_REQUEST_TIMEOUT_MS`, and `resolveConnectOptions`.
- `Subscription`: event-line stream after a successful `subscribe`.
- `decodeProtocolEvent`: decodes one event envelope into the generated typed
  event union when the event name is known.
- `CatchAllEvent`: forward-compatible shape for unknown event names.
- `RawStream`: `ReadableStream<Uint8Array>` plus `WritableStream<Uint8Array>`
  attach duplex.
- `ClientError`: structured SDK error with `toProtocolError()`.
- `ClientErrorClass`, `ClientErrorCode`, and `ClientErrorKind`: the SDK error
  taxonomy used by `ClientError`.
- `Request`, `Response`, `OkResponse`, `ErrResponse`, and `Event`: hand-written
  control envelopes used by the runtime SDK layer.
- `decodeResponse`, `isRequest`, `isOkResponse`, `isErrResponse`, and `isEvent`:
  envelope guards and decoders for low-level callers and tests.
- Raw and attach helpers: both entries export `connectRawWs`, `attachRawWs`,
  `connectRawTransport`, and `attachRawTransport`; the root entry additionally
  exports `connectRawLocal`, `connectRawTcp`, `attachRaw`, `attachRawLocal`, and
  `attachRawTcp`.
- Re-export of every symbol from `@pohunek/protocol`, including generated domain
  types, `Methods`, `ProtocolEvent`, `EventName`, `AttachPrelude`,
  `PROTOCOL_VERSION`, `MAX_CONTROL_LINE_BYTES`, `EVENT_NAMES`, and individual
  event-name constants. Generated files under `web/shared/src/generated/**` are
  refreshed only by `cargo xtask ts generate`, never hand-edited.

Supported runtimes:

- Bun: supports the direct socket transport and the WebSocket relay transport.
- Node >= 18: supports the direct Unix/TCP socket transport through `node:net`.
- Node >= 22: supports the WebSocket relay transport through the built-in WHATWG
  `WebSocket` global.
- Browser: import `@pohunek/sdk/browser`; it supports only the WebSocket relay
  transport because browsers cannot dial daemon Unix sockets or NetBird TCP
  directly.

Connection APIs:

- `Client.defaultOptions()`: returns resolved default timeouts.
- `Client.connectWs(baseUrl, host, opts?)`: WebSocket relay. `baseUrl` may use
  `http`, `https`, `ws`, or `wss`; the SDK connects to
  `/daemon/<host>/control` under that base URL.
- `Client.connectTransport(transport, opts?, remoteHost?)`: injection point for
  tests and custom transports that implement `Transport`.
- `connectLocal(socketPath, opts?)` and `connectTcp(host, {host, port}, opts?)`:
  root-entry Bun/Node helpers for a direct Unix socket or daemon TCP address.
- `SocketTransport.unix(socketPath, opts?)` and `SocketTransport.tcp(host,
  {host, port}, opts?)`: construct direct socket transports.
- `WsTransport.relay(baseUrl, host, opts?)`: constructs the WebSocket relay
  transport for `/daemon/<host>/control` and `/daemon/<host>/attach`.

Request APIs:

- `client.call(method, params)`: typed call keyed by the generated `Methods`
  map.
- `client.handshake()`: calls `daemon.health` and enforces strict protocol
  version equality.
- `client.request(request)`: sends one raw request envelope and returns the
  `ok` payload.
- `client.subscribe(request)`: consumes the control connection after the
  subscribe ack and returns `Subscription`.
- `client.close()`: closes the control channel.
- `subscription.nextLine()`: returns raw event JSON text or `null` on close.
- `subscription.nextEvent()`: returns `ProtocolEvent | CatchAllEvent | null`.
  Known event names decode to the generated typed union; unknown event names are
  preserved as `CatchAllEvent` rather than rejected, so older clients tolerate
  additive daemon events.

Attach APIs:

- Root-entry `connectRawLocal` and `connectRawTcp`, plus shared `connectRawWs`,
  open unframed raw byte channels without writing the attach prelude.
- Root-entry `attachRaw(host, socketPath, streamId, opts?)` mirrors the Rust
  convenience helper for local hosts (`""` or `"local"`). The TypeScript SDK
  core does not perform NetBird host resolution; remote callers pass an
  explicit address to `attachRawTcp` or use the WebSocket relay with
  `attachRawWs`.
- Root-entry `attachRawLocal` and `attachRawTcp`, plus shared `attachRawWs`, open
  a raw channel, write exactly one attach prelude, parse a failed redemption
  response as `ClientError`, and otherwise return the raw attach stream.

SDK error mapping:

- Daemon protocol errors are preserved as `ClientError.kind === "protocol"` or
  `"remoteProtocol"` and retain the daemon's original `class`, `code`, `msg`,
  and `recover` fields.
- SDK-originated errors map into the public protocol taxonomy through
  `ClientErrorClass` and `ClientErrorCode`: `daemon_unreachable`, `framing`,
  `host_unreachable`, `remote_daemon_unavailable`, `io_error`, `json_error`, and
  `version_mismatch`.
- `ClientError.toProtocolError()` returns the structured `ProtocolError` for
  CLI/API rendering, and `recoverHint()` returns the optional recovery text.

The `@pohunek/backend` package (renamed from `@pohunek/relay`) exports the
transport-core server used by `WsTransport`:
`startRelay({bindHost, port, targets, allowLoopbackBind?})`,
`validateRelayBindAddr`, `isNetbirdIp`, `RelayBindAddrError`, and the
`DaemonTarget`/`RelayHandle` types. The package rename and its added host
discovery, `/api/hosts`, and SPA composition do not change the WebSocket relay
framing contract documented above: it remains a transparent 1:1 tunnel.
