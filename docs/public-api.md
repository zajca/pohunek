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

The current public protocol version is `2` (`PROTOCOL_VERSION`), and this build
supports the inclusive range `2..=2` (`SUPPORTED_PROTOCOL_VERSIONS`). Requests
carry `v: {minimum, maximum}`. The first valid response selects the highest
overlapping version as an integer `v`, and that selection is fixed for the
lifetime of the connection. Subscription events use the same selected version.
A non-overlapping range returns `daemon/version_mismatch`.

The former exact integer request envelope is deliberately rejected. Protocol
v2 is a one-time coordinated pre-1.0 boundary: every CLI, GUI, web backend/SDK,
custom client, and local or remote daemon that communicates with another peer
must be upgraded together. There is no v1 envelope or notification-policy
compatibility shim. After crossing this boundary, future peers negotiate the
highest overlapping range instead of requiring equal maximum versions.

Clients should call `daemon.health` after opening a control connection to learn
the daemon build version and protocol version, but `daemon.health` is not a
special unauthenticated handshake. It is an ordinary request and is negotiated
like every other method.

Within a negotiated public version, additive changes do not require a bump only
where the containing contract is explicitly open. New methods and error codes
are additive; older daemons return `daemon/method_not_found` for unknown
methods. Optional fields retain their documented omission behavior. Envelope,
observation, native-report, capability, and notification-policy objects are
strict and reject unknown fields, so changing their accepted shape requires the
appropriate negotiated-version treatment. `AgentKind` and provider-policy map
keys are deliberately open value namespaces, not open object shapes.

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
{"v":{"minimum":2,"maximum":2},"id":"req-7f3","method":"session.list","params":{}}
```

Fields:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `v` | object | yes | Inclusive `{minimum, maximum}` range. Endpoints are non-zero integers, `minimum <= maximum`, and unknown range fields are rejected. |
| `id` | string | yes | Correlation id. The response echoes it. |
| `method` | string | yes | One of the public method names below. |
| `params` | JSON value | no | Method-specific params. Missing defaults to `null`. |
| `origin_session_id` | string | no | Session containing the caller process. Must be paired with `origin_daemon_id`. |
| `origin_daemon_id` | string | no | Daemon instance paired with `origin_session_id`. Must be paired with it. |

For parameterless methods, send `params: null` or omit `params` unless a method
documents another defaultable object.

Origin markers are either both absent or both present, non-empty, bounded, and
restricted to unescaped ASCII identifier characters. Managed children inherit
the pair; the Rust SDK propagates inherited origin and the TypeScript SDK
propagates an explicitly configured `ConnectOptions.origin` to normal,
subscription, and dedicated connections. When both markers identify the target
as the caller's own origin session, the daemon returns
`runtime/plugin_self_target_denied` for exactly `session.stop`,
`session.resume`, `session.remove`, `session.fork`, `session.resize`,
`session.set_metadata`, `session.rename`, and `session.input`. Read-only methods,
including observation, remain available. The lifecycle reports
`session.report_agent`, `session.release_agent`, and `session.report_native_id`
are explicitly allowed because hooks must report their own session; the public
native-id report is the necessary local fallback when the owner-private worker
claim cannot be delivered. This is a narrow server-side confused-deputy guard
inside the existing single-operator trust boundary, not per-session
authentication or a broader mutation policy.

### Response

Successful response:

```json
{"v":2,"id":"req-7f3","ok":{"status":"ok"}}
```

Error response:

```json
{
  "v": 2,
  "id": "req-7f3",
  "err": {
    "class": "daemon",
    "code": "method_not_found",
    "msg": "unknown control method: example.missing"
  }
}
```

Exactly one of `ok` or `err` is present.
The integer `v` is the highest overlapping version selected by the first
response and cannot change on later responses on the same connection.

### Event

Events are pushed only after a successful `subscribe` request. They are also
newline-delimited JSON, one event per line:

```json
{"v":2,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title"}
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
| `session.new` | `SessionNewParams` | `SessionNewResult` | Starts an agent PTY session. Bare and profile-based Hermes launches first require an executable whose isolated, bounded `--version` probe matches the pinned supported release; failure returns payload-free `agent_runtime_unsupported` before session, worker, or worktree creation. Optional `metadata` is written atomically with the session (see the `metadata` field note under `SessionInfo` below); the CLI exposes it as repeatable `--meta key=value`. |
| `session.list` | `SessionListParams` or `null` | `Vec<SessionInfo>` | Lists sessions; filters use AND semantics. |
| `session.inspect` | `SessionId` | `SessionInfo` | `SessionId` is a JSON string, e.g. `"s-1"`. |
| `session.stop` | `SessionId` | `SessionStopResult` | Stops a live session (the entry stays in `list`). |
| `session.resume` | `SessionId` | `SessionResumeResult` | Explicitly recovers a terminal or lost logical session from captured native recovery metadata, reusing the logical session id but creating a new worker and runtime generation. Hermes recovery revalidates the frozen executable against the pinned release before any recovery write or worker launch and returns payload-free `agent_runtime_unsupported` when unavailable or incompatible. Live, reconnecting, conflicting, or incompatible runtimes are rejected; sessions without native metadata return `not_resumable` or `agent_not_resumable`. Daemon restart never calls this method automatically. |
| `session.fork` | `SessionForkParams` | `SessionForkResult` | Forks a native agent conversation into a new pohunek session id and PTY, using the source session's cwd/worktree for `cwd_mode: "same"`. Live sources are allowed. Unknown ids return `session_not_found`; external sessions return `session_external_read_only`; sources without launch-agent native metadata return `not_resumable` or `agent_not_resumable`; Codex- and Hermes-backed sessions return `agent_fork_unsupported`. A successful fork emits `session_created`. |
| `session.remove` | `SessionId` | `SessionRemoveResult` | Evicts a session from the registry, stopping it first if still live. Unknown id is `session_not_found`. |
| `session.runtime_inventory` | `null` | `RuntimeInventoryResult` | Returns the durable-worker runtime inventory captured at startup reconciliation: one `RuntimeInventoryEntry` per discovered worker with its runtime slot, claimed session id, worker/runtime ids, and classification (`managed`, `orphaned`, `conflict`, `incompatible`, or `identity_mismatch`). Read-only operator diagnostic; it never mutates or kills a worker. |
| `session.attach` | `SessionAttachParams` | `SessionAttachResult` | Mints a one-shot attach stream id. |
| `session.detach` | `SessionDetachParams` | `SessionDetachResult` | Cancels an active attach stream. After a worker stream failure, the first call returns its optional typed `error` and consumes that short-lived result; unknown or already-consumed streams return `detached: false` without `error`. |
| `session.resize` | `SessionResizeParams` | `SessionResizeResult` | Resizes the PTY on the control connection. |
| `session.input` | `SessionInputParams` | `SessionInputResult` | Injects text using agent-specific input framing. Hermes accepts at most `MAX_SESSION_INPUT_BYTES` UTF-8 bytes, permits LF and tab but rejects other C0/C1 controls without rewriting them, and returns `session_input_blocked` for fire-and-forget input while approval-visible activity is blocked. Unsafe or oversized Hermes text returns `session_input_rejected`. Every per-session input holds one gate through its complete framing transaction, preventing waited/waited and waited/fire-and-forget interleaving. Every worker plan contains a body fragment with the provider `delay_after_ms` and a separate submit fragment; body and Enter are never merged into one paste burst. With `wait`, Rust and TypeScript SDKs normalize absent `until` to `[]` before wire validation and reject `timeout_ms` outside `1..=8000` before transport. The daemon deduplicates targets, acquires a waiter permit, starts the overall deadline before the input gate, and rejects blocked activity with `session_agent_blocked` both before the gate and at the causal boundary, independently of provider fire-and-forget policy. A worker-owned submit delay cannot be activity-revalidated and already-written text cannot be safely retracted from an arbitrary TUI, so waited input rejects nonzero-delay framing with `session_input_wait_unsupported` before writing bytes. Zero-delay framing captures runtime-scoped activity revision evidence immediately before the atomic two-fragment plan. Matching evidence above that lower bound and observed through the fixed deadline succeeds, including evidence between PTY plan flush and worker ACK. Timed-out worker exchanges consume their late ACK to preserve control framing; after the plan is sent delivery outcome may be unknown, so callers inspect the session and do not retry blindly. Bounded evidence history retains the maximum wait window, so a later same-activity report cannot erase valid pre-deadline evidence. The result includes `activity`, `activity_source`, `runtime`, `activity_epoch`, and decimal-string `activity_revision`; SDK helpers require all five and return `session_input_wait_contract_mismatch` when a same-version daemon ignores `wait` or omits evidence. Clients deduplicate by `(activity_epoch, runtime, activity_revision)`. Runtime exit returns `session_not_running`; replacement returns `session_runtime_changed`; external sessions return `session_external_read_only`; shutdown cancels delivery or waiting. The dedicated connection adds fixed response headroom beyond the overall deadline. |
| `session.screen` | `SessionScreenParams` | `SessionScreenResult` | Reads one bounded, rendered, runtime-bound terminal snapshot without acquiring attach ownership or resizing the terminal. |
| `session.output` | `SessionOutputParams` | `SessionOutputResult` | Reads a newest retained tail or continues from an exact runtime-scoped output cursor. A request with `wait_ms` uses a dedicated connection with bounded-wait headroom. |
| `session.wait` | `SessionWaitParams` | `SessionWaitResult` | Performs one bounded long poll for state, activity, metadata, terminal, output, or runtime change. It always uses a dedicated connection. |
| `session.report_agent` | `SessionReportAgentParams` | `SessionReportAgentResult` | Hook callback for nested agents running inside an existing session. It records an active-agent claim, optional process binding, and optional active native metadata without changing launch identity or resume binding; ignored reports return `recorded: false`. Claims are reconciled with process facts and can be auto-released when no live backing process remains. |
| `session.release_agent` | `SessionReleaseAgentParams` | `SessionReleaseAgentResult` | Hook callback that clears a matching active nested-agent report and restores the session's default detector identity. Claude `SessionEnd` hooks use this as the clean-exit fast path; non-current releases return `released: false`; process-backed auto-release uses the same clear path. |
| `session.report_native_id` | `SessionReportNativeIdParams` | `SessionReportNativeIdResult` | Hardened public fallback for launch-agent resume metadata. Reports are runtime- and process-bound, ordered, expiring, and provider-matched; ignored reports return `recorded: false`. The owner-private worker claim path is preferred. This is not the nested-agent active identity callback. |
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
| `notification.policy.set` | `NotificationPolicyParams` | `NotificationPolicyResult` | Validates, replaces, and persists the daemon notification and automatic-retention policy at `<data_dir>/notifications/policy.json`. |
| `notification.retention.prune` | `NotificationRetentionParams` or `null` | `NotificationRetentionResult` | Explicitly deletes records selected by retention filters, or reports matches when `dry_run` is true. Applied pruning may also compact the action log at the policy threshold. |
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

`HostCapabilities` advertises `terminal_read_supported`,
`output_read_supported`, and `session_wait_supported` independently. Clients
must check the relevant flag instead of assuming that a reachable daemon or an
attach-capable session supports every observation method. Its `runtimes` entries
are live host-local probes: `agent` is the selected profile or base name and
optional `agent_base` identifies the compiled adapter behind it. Optional
`version` and `supported` are a provider policy, not generic availability:
their absence means that no version policy applies. For Hermes, `available:
false` omits both fields, while an installed unparseable or non-`0.20.0`
executable reports `supported: false`. The daemon independently enforces the
same policy immediately before every Hermes launch or recovery rather than
trusting a potentially stale inventory response. The probe clears ambient
environment state and uses private temporary HOME, Hermes, XDG, Python-cache,
and working directories. Its executable is resolved and canonicalized once;
the exact absolute path is then passed to the worker without a second PATH
lookup. The single-operator trust boundary still permits the same owner to
replace that canonical file between probe and exec; eliminating that residual
would require an fd-based execution contract. An absent `agent_base` is compatible with legacy custom
profile inventory, but a present unknown base is presentation-only and not
launchable.

### Daemon Runtime Configuration

`POHUNEK_OBSERVE_EXTERNAL_AGENTS` is an opt-in daemon environment flag. Accepted
true values are `1`, `true`, `yes`, and `on`; accepted false values are `0`,
`false`, `no`, `off`, or an unset variable. When true, the daemon watches the
operator's Claude and Codex transcript trees and same-user process table for
agents started outside pohunek. The corresponding `SessionRegistryConfig`
setting is `observe_external_agents`, default `false`.

Observation limits are validated together when the session registry starts.
Defaults are 783,240 raw output bytes, an 8,000 ms output wait, an 8,000 ms
session wait, 200 rows, 500 columns, a 1,048,418-byte serialized screen result,
128 global waiters, and 8 waiters per session. Values must be non-zero, must not
exceed the shared protocol ceilings, and the per-session waiter cap must not
exceed the global cap. Invalid combinations fail fast with
`runtime/observation_limits_invalid`; the daemon does not silently substitute
defaults.

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
- `capabilities`: required `SessionCapabilities` object with independent
  `resume` and `fork` booleans frozen for the logical session. Clients must use
  these flags instead of inferring provider behavior from `agent` or
  `agent_base`. Records that predate the field load both flags as `false`.
- `name`: optional owner-set display name; absent means the session is shown by
  its id. Set at `session.new` and changed via `session.rename`.
- `agent`: profile name.
- `agent_base`: `shell`, `codex`, `claude`, or `hermes`; unknown wire values
  remain presentation-only.
- `active_agent`: optional runtime agent profile currently active inside the
  session. Present for nested agents reported through hooks or inferred from
  process facts.
- `active_agent_base`: optional runtime base kind (`shell`, `codex`, `claude`,
  or `hermes`) for `active_agent`.
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
- `runtime`: optional durable runtime object. It is absent for observed external
  sessions and peers predating worker-backed sessions. `runtime_generation` is
  a canonical unsigned decimal JSON string, not a JSON number. `runtime.state` is
  `starting`, `live`, `reconnecting`, `terminal`, `lost`, `conflict`, or
  `incompatible`; `worker_id` identifies the PTY owner and `runtime_id`
  identifies the PTY generation. `started_at`, `last_connected_at`, and
  `loss_reason` are optional. Daemon reconnection preserves both identities;
  explicit native recovery changes them.
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
  also resumable. Hermes resumes only when this valid reference exists, as
  `hermes chat --resume <reference>`; it never infers an ambient Hermes
  session.
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

`AgentKind` wire values are forward-compatible for presentation. An unknown
string round-trips through Rust and TypeScript clients as a neutral value, but
it is rejected with `runtime/agent_kind_unsupported` by agent-targeted mutation
and persistence paths. Unknown values never silently become a supported launch,
resume, or fork adapter.

### Session Observation

Observation is available only for Pohunek-managed terminals. It does not attach,
take input ownership, or change terminal dimensions. `runtime_generation`, all
output offsets, terminal watermarks, process-start identities, and report
sequences are canonical unsigned decimal strings. They remain exact beyond
JavaScript's safe-integer range and reject signs, whitespace, overflow, and
redundant leading zeroes.

`session.screen` accepts:

```json
{"session_id":"s-42"}
```

A successful result has this exact shape (optional `title` and `progress` may be
omitted):

```json
{
  "session_id": "s-42",
  "worker_id": "worker-1",
  "runtime_id": "runtime-1",
  "runtime_generation": "3",
  "watermark": "7",
  "dimensions": {"cols": 80, "rows": 24},
  "cursor": {"row": 1, "col": 4, "visible": true},
  "alternate_screen": false,
  "title": "terminal",
  "visible_lines": ["pohunek"]
}
```

Visible lines are plain UTF-8 terminal cells with control sequences removed.
The protocol ceiling for the serialized result is
`MAX_SESSION_SCREEN_RESPONSE_BYTES` (1,048,418 bytes), derived from the 1 MiB control-line cap
with response-envelope headroom. The daemon additionally defaults to at most
200 rows and 500 columns. Oversize results return the payload-free
`runtime/session_output_limit_exceeded` error.

`session.output` uses an optional nested runtime identity and an exclusive
cursor. Omitting `after_offset` requests the newest retained tail. A cursor
requires its exact runtime identity, and `wait_ms` requires a cursor:

```json
{"session_id":"s-42","max_bytes":65536}
```

The initial-tail request deliberately has no runtime or offset. Persist the
returned `runtime_id`, `runtime_generation`, and `next_offset` before issuing a
cursor-based read:

```json
{
  "session_id": "s-42",
  "runtime": {"runtime_id": "runtime-1", "runtime_generation": "3"},
  "after_offset": "2",
  "max_bytes": 65536,
  "wait_ms": 5000
}
```

The result returns standard base64 and every cursor needed to continue:

```json
{
  "session_id": "s-42",
  "runtime_id": "runtime-1",
  "runtime_generation": "3",
  "history_start_offset": "4",
  "start_offset": "4",
  "next_offset": "6",
  "runtime_end_offset": "6",
  "data_base64": "AJ8=",
  "gap": {"start_offset": "2", "end_offset": "4"},
  "has_more": false,
  "timed_out": false
}
```

`gap` is omitted unless the requested retained history was evicted. Continue
immediately while `has_more` is true. The shared raw-data ceiling is
`MAX_SESSION_OUTPUT_BYTES` (derived from the 1 MiB line limit after base64 and
metadata headroom) is 783,240 raw bytes; the daemon may configure a lower
positive value. `max_bytes`
must be `1..=MAX_SESSION_OUTPUT_BYTES`. `wait_ms` must be `1..=8000` and is used
only when the explicit cursor is at the current end. A waiting output read uses
a dedicated SDK connection. In the shown gap result, offsets `2..4` are no
longer retained: callers must discard the old cursor and restart from a fresh
screen or newest tail, never synthesize the missing bytes.

`session.wait` requires a non-zero `timeout_ms` no greater than 8000 and at
least one predicate. Runtime-scoped terminal/output cursors require `runtime`;
`after_updated_at` is RFC 3339; present `states` and `activities` arrays cannot
be empty:

```json
{
  "session_id": "s-42",
  "runtime": {"runtime_id": "runtime-1", "runtime_generation": "3"},
  "after_updated_at": "2026-08-04T10:00:00Z",
  "after_terminal_watermark": "7",
  "after_output_offset": "8",
  "states": ["stopped"],
  "activities": ["blocked"],
  "timeout_ms": 8000
}
```

It returns `reason`, the current redacted `SessionInfo`, and optional current
`terminal_watermark` / `output_offset`. Reasons are `state_matched`,
`activity_matched`, `session_updated`, `terminal_changed`, `output_advanced`,
`runtime_changed`, or `timeout`. Registration follows snapshot-register-recheck
and holds no registry write lock while sleeping. Each wait uses a dedicated
connection and consumes one waiter slot; defaults are 128 concurrent waiters
globally and 8 per session. Disconnect is not promised as immediate daemon-side
cancellation: the required timeout is the resource-release bound.

A wake and a timeout use the same result shape; callers branch only on the
typed reason:

```json
{
  "reason": "output_advanced",
  "session": {
    "id": "s-42",
    "external": false,
    "capabilities": {"resume": false, "fork": false},
    "agent": "shell",
    "agent_base": "shell",
    "cwd": "/workspace/project",
    "cwd_source": "launch",
    "pid": 4242,
    "cols": 120,
    "rows": 40,
    "state": "running",
    "state_source": "process",
    "warnings": [],
    "metadata": {},
    "created_at": "2026-06-17T10:00:00Z",
    "updated_at": "2026-06-17T10:01:00Z"
  },
  "terminal_watermark": "8",
  "output_offset": "9"
}
```

```json
{
  "reason": "timeout",
  "session": {
    "id": "s-42",
    "external": false,
    "capabilities": {"resume": false, "fork": false},
    "agent": "shell",
    "agent_base": "shell",
    "cwd": "/workspace/project",
    "cwd_source": "launch",
    "pid": 4242,
    "cols": 120,
    "rows": 40,
    "state": "running",
    "state_source": "process",
    "warnings": [],
    "metadata": {},
    "created_at": "2026-06-17T10:00:00Z",
    "updated_at": "2026-06-17T10:01:00Z"
  },
  "terminal_watermark": "7",
  "output_offset": "8"
}
```

The timeout means that no selected predicate changed before the requested
deadline. It does not imply a healthy, idle, or terminal session.

| Result field | Type | Notes |
|---|---|---|
| `reason` | `SessionWaitReason` string | The first satisfied reason, or `timeout`. |
| `session` | `SessionInfo` | Current redacted public snapshot, including `capabilities`. |
| `terminal_watermark` | decimal string, optional | Current rendered-terminal revision when a managed terminal is available. |
| `output_offset` | decimal string, optional | Current exclusive output end when a managed terminal is available. |

Observation errors are stable and payload-free: `session_terminal_unavailable`,
`session_has_no_managed_terminal`, `session_runtime_changed`,
`session_output_limit_exceeded`, `session_wait_limit_exceeded`,
`session_waiter_limit_reached`, and `worker_feature_unavailable`. Restart from a
fresh screen/tail after runtime change. A worker on the immediately preceding
private protocol remains usable for existing lifecycle and attach operations,
but observation returns `worker_feature_unavailable`.

### Active-Agent Hook Payloads

Managed PTY children inherit these reserved environment values:

- `POHUNEK_ENV=1`
- `POHUNEK_SESSION_ID`
- `POHUNEK_WORKER_ID`
- `POHUNEK_WORKER_SOCKET_PATH`
- `POHUNEK_WORKER_PROTOCOL_VERSION`
- `POHUNEK_SOCKET_PATH` for daemon-targeted notification delivery

Identity hooks prefer the owner-private worker endpoint so an accepted launch
or active identity is retained while the daemon is unavailable. Notification
hooks continue to use the public daemon socket; notifications produced during a
daemon outage are not durable. `POHUNEK_DAEMON_ID` remains additive compatibility
data but is not the stable runtime identity and must not be used for
self-feedback decisions by new clients.

When the worker-private native-identity claim cannot be delivered, shipped
Codex and Claude hooks must retain the necessary local fallback to the public
`session.report_native_id` method. The origin-session guard deliberately allows
this lifecycle report to target its own session. Its
strict params are `session_id`, `runtime_id`, `agent`, non-zero `pid`, decimal
string `pid_start_identity`, decimal string monotonic `sequence`, RFC 3339
`expires_at`, `native_session_id`, and optional `transcript_path`. The daemon
records only an unexpired claim for the current logical session/runtime whose
agent matches the frozen launch profile or base kind, whose PID and kernel
start identity match the launch process, and whose sequence is newer than the
last accepted claim. Stale runtime, PID reuse, expired, duplicate/out-of-order,
wrong-provider, and wrong-session reports are ignored. Native identifiers and
transcript paths are redacted from `Debug` and must not enter logs or errors.
The claim lifetime is capped at 60 seconds from receipt.

```json
{
  "session_id": "s-42",
  "runtime_id": "runtime-42",
  "agent": "codex",
  "pid": 4242,
  "pid_start_identity": "7",
  "sequence": "1",
  "expires_at": "2026-08-04T10:00:00Z",
  "native_session_id": "provider-native-id"
}
```

The result is exactly `{"recorded":true}` or `{"recorded":false}`.

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
- `enabled`: base per-kind flags used when a provider has no explicit entry.
- `providers`: optional deterministically ordered object mapping provider wire
  names to complete per-kind overrides. Missing keys fall back to `enabled`.
- `retention`: automatic daemon-owned cleanup and physical-compaction settings.
  Persisted policies without this additive field receive the defaults.

For example:

```json
{
  "attention_dedupe_window_secs": 120,
  "attention_debounce_secs": 5,
  "enabled": {
    "agent_blocked": true,
    "approval_required": true,
    "turn_completed": false,
    "session_finished": false,
    "error": true,
    "system": false
  },
  "providers": {
    "claude": {
      "agent_blocked": true,
      "approval_required": true,
      "turn_completed": true,
      "session_finished": false,
      "error": true,
      "system": false
    },
    "hermes": {
      "agent_blocked": true,
      "approval_required": true,
      "turn_completed": false,
      "session_finished": false,
      "error": true,
      "system": false
    }
  },
  "retention": {
    "sweep_interval_secs": 21600,
    "info_ttl_secs": 259200,
    "warning_ttl_secs": 1209600,
    "resolved_attention_ttl_secs": 604800,
    "resolved_error_ttl_secs": 2592000,
    "archived_ttl_secs": 7776000,
    "compaction_min_actions": 1000
  }
}
```

Provider names are open strings so adding a provider does not change this wire
shape. The former fixed `codex` / `claude` fields are not accepted and have no
compatibility shim.

Default policy enables `agent_blocked`, `approval_required`, and `error`.
`turn_completed`, `session_finished`, and `system` are implemented but disabled
by default. The daemon materializes complete default entries for `codex`,
`claude`, and `hermes`; a missing provider key still falls back to `enabled`.

Every retention duration and `compaction_min_actions` must be greater than zero.
The daemon runs one sweep at startup and then every `sweep_interval_secs` using
the current policy. Informational/success, warning, acknowledged attention,
acknowledged error, and archived records use their respective TTLs. Unread or
read action-required and error records are never deleted automatically. After
eligible records receive normal deletion events, the store atomically rewrites
the action log to one current action per non-deleted record once the configured
action threshold is reached.

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

When a session enters `working`, or reaches a terminal lifecycle state, the daemon resolves both
`attention:<session_id>` and `turn:<session_id>`. Pending records with those
keys are dropped before they ever persist, and already-visible unread/read
matching records are acknowledged with `notification_updated`. An `idle`
observation alone does not resolve attention because a live approval prompt can
be technically idle while still requiring owner input.

#### Session notification debounce

`agent_blocked`, `approval_required`, and session-scoped `turn_completed` are
held pending rather than persisted immediately when they carry
`attention:<session_id>` or `turn:<session_id>`. The daemon mints the
notification id and returns `notification.create` result `created: true` with
the full record, but the record is not written to the store and does not appear
in `notification.list` until it flushes. No `notification_created` event is
emitted for a pending record.

The daemon holds the pending record for `attention_debounce_secs`. If the
session enters `working` or reaches a terminal lifecycle state within that
window, the pending record is dropped entirely: nothing is ever persisted, and no event fires. Only if the
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
while the normal `idle_prompt` creates no notification. `auth_success`,
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
| `configuration` | `paths_unavailable`, `netbird_invalid_config`, `invalid_discovery_options` |
| `daemon` | `version_mismatch`, `method_not_found`, `bad_request`, `daemon_unreachable`, `remote_daemon_unavailable`, `session_input_wait_contract_mismatch`, `projects_not_configured`, `serialize_failed`, `json_error`, `project_task_panicked`, `doctor_task_panicked`, `assistant_materialize_task_panicked`, `assistant_method_unsupported`, `attach_self_feedback` |
| `transport` | `framing`, `host_unreachable`, `request_timeout` |
| `discovery` | `netbird_cli_missing`, `netbird_state_unavailable`, `host_unknown`, `remote_discovery_failed` |
| `runtime` | `agent_binary_missing`, `agent_profile_not_found`, `invalid_profile`, `agent_not_resumable`, `not_resumable`, `invalid_session_ref`, `no_capable_agent`, `bundle_unavailable`, `assistant_bundle_mismatch`, `materialization_failed`, `agent_cannot_read_bundle`, `session_not_found`, `session_not_running`, `session_not_terminal`, `session_external_read_only`, `session_exit_timeout`, `session_runtime_commit_stale`, `attach_not_found`, `attach_expired`, `worker_attach_stream_failed`, `worker_protocol_incompatible`, `worker_controller_busy`, `worker_identity_mismatch`, `worker_invalid_state`, `worker_invalid_request`, `worker_invalid_data_token`, `worker_write_outcome_unknown`, `worker_runtime_fault`, `client_file_descriptors_exhausted`, `system_file_descriptors_exhausted`, `pty_alloc_failed`, `spawn_failed`, `pty_error`, `io_error`, `project_store_error`, `project_detect_failed`, `not_a_git_repo`, `project_not_found`, `project_ambiguous`, `prompt_not_found`, `template_not_found`, `action_not_found`, `invalid_name`, `invalid_template`, `invalid_action`, `path_escape`, `config_read_failed`, `agent_not_installable`, `agent_config_dir_missing`, `integration_settings_invalid`, `integration_io_failed`, `worktree_store_error`, `worktree_path_conflict`, `invalid_base_branch`, `worktree_branch_in_use`, `worktree_add_failed`, `invalid_branch`, `invalid_branch_slug`, `notifications_not_configured`, `notification_task_panicked`, `notification_store_error`, `notification_not_found`, `invalid_notification_transition`, `invalid_notification_metadata`, `invalid_notification_session_id`, `invalid_notification_dedupe_key`, `notification_kind_disabled`, `invalid_notification_timestamp`, `invalid_notification_cursor`, `invalid_notification_policy` |

Protocol v2 additionally emits these runtime codes for provider-neutral agent
and observation behavior: `agent_kind_unsupported`,
`agent_fork_unsupported`, `session_terminal_unavailable`,
`session_has_no_managed_terminal`, `session_runtime_changed`,
`session_output_limit_exceeded`, `session_wait_limit_exceeded`,
`session_waiter_limit_reached`, `worker_feature_unavailable`,
`plugin_self_target_denied`, `agent_runtime_unsupported`,
`session_input_rejected`, `session_input_blocked`, `session_agent_blocked`,
`session_input_invalid_wait`, `session_input_wait_unsupported`, and
`session_input_timeout`. Daemon startup may additionally return
`observation_limits_invalid`. Observation request errors intentionally carry no terminal
payload or current-runtime payload; refresh `session.inspect` or restart
observation from a fresh screen/tail when recovery requires new coordinates.

`session_input_wait_contract_mismatch` means the SDK cannot prove whether a
same-version daemon honored the wait contract. Delivery outcome is unknown: do
not retry blindly. Upgrade daemon and client together, inspect the session, and
only resend when the observed terminal state proves the original input was not
applied.

`session_input_timeout` also treats delivery as potentially unknown. Inspect the
current session before deciding whether to resend, and do not retry blindly.

`session_runtime_commit_stale` means a lifecycle or runtime transition lost a
concurrent durable commit: another runtime is already authoritative for the
logical session because the candidate generation is older or a different
runtime owns the same generation. The losing candidate is not published to the
in-memory registry or subscribers and emits no success event. Refresh with
`session.inspect`, then retry only if the operation is still valid against the
current authoritative runtime identity, generation, and state; never reuse the
losing candidate's stale runtime coordinates.

This code does not report post-rename durability uncertainty. If the atomic
rename made a session record authoritative but syncing the parent directory
then failed, the daemon treats the commit as applied and logs a sanitized
durability warning internally. That condition is not returned as
`session_runtime_commit_stale`.

Clients must not parse `msg`. Branch on `class` and `code`, then display `msg`
and `recover` for unknown codes.

## Events

`subscribe` turns the control connection into a one-way event stream after this
ack:

```json
{"v":2,"id":"sub-1","ok":{"subscribed":true}}
```

The daemon then writes these events:

| Event | Payload | Meaning |
|---|---|---|
| `session_created` | `{session: SessionInfo}` | A new logical session was created or forked. Daemon reconnection and native recovery use their dedicated runtime events. |
| `session_updated` | `{session: SessionInfo}` | Session metadata, active-agent report/release, cwd/worktree/project association, state, resize, resume binding, or terminal state changed. |
| `session_stopped` | `{session: SessionInfo}` | A user-requested stop completed. |
| `session_removed` | `{session: SessionInfo}` | A session was evicted from the registry; clients drop it from their view. |
| `session_runtime_reconnected` | `{session: SessionInfo}` | A replacement daemon adopted the same worker and runtime generation. This is not a new session and does not imply provider-native recovery. |
| `session_runtime_lost` | `{session: SessionInfo}` | The worker or host runtime is gone. The logical record remains visible and may support explicit recovery. |
| `session_runtime_conflict` | `{session: SessionInfo}` | Runtime discovery found duplicate, mismatched, or otherwise ambiguous live identity. The daemon quarantines the conflict and does not kill a worker automatically. |
| `session_runtime_discovered` | `{entry: RuntimeInventoryEntry}` | Startup reconciliation classified a discovered durable worker that is not a plainly managed runtime (orphaned, conflicting, incompatible, or identity-mismatched). Emitted once per non-managed discovery so operators can inspect quarantined runtimes. |
| `session_native_recovered` | `{session: SessionInfo, previous_runtime_id?: string, runtime_id?: string}` | Explicit provider-native recovery created a new worker and runtime generation for the same logical session. `previous_runtime_id` can be absent for a one-time migrated legacy session; production worker recovery includes the new `runtime_id`. |
| `agent_state` | `{session_id: SessionId, activity: AgentActivity, source: StateSource, runtime?: SessionRuntimeIdentity, activity_epoch?: string, revision?: ActivityRevision}` | Agent activity changed. `source` may be `report` when a hook report supplied explicit active-agent state. Current daemons emit `runtime`, `activity_epoch`, and decimal-string `revision`, making `(activity_epoch, runtime, revision)` exact reconnect-safe evidence rather than a hint to re-read only the latest snapshot; the fields remain additive for general v2 subscribers, while input-wait success requires them through `SessionInputResult`. |
| `attach_opened` | `{session_id: SessionId, stream_id: string}` | A pending attach token was redeemed and a raw stream opened. |
| `attach_closed` | `{session_id: SessionId, stream_id: string}` | A raw attach stream ended or was detached. |
| `notification_created` | `{record: NotificationRecord}` | A durable notification record was created. |
| `notification_updated` | `{record: NotificationRecord}` | A notification record changed lifecycle status, was upgraded by higher-priority source dedupe, or was acknowledged by resolve/supersede processing. |
| `notification_deleted` | `{notification_id: NotificationId}` | A notification record was logically deleted. |

Subscription connections ignore further client input after the ack. A slow
subscriber may miss older events if the daemon's internal event channel lags; it
should reconcile by calling `session.list`, `session.inspect`, or
`notification.list`.

## CLI Process API

Commands with `--json` write exactly one pretty-printed document to stdout and
reserve stderr for diagnostics. Success exits zero with:

```json
{
  "cli_version": "0.x.y",
  "protocol": {"minimum": 2, "maximum": 2},
  "ok": {}
}
```

A typed operational or usage failure exits non-zero and replaces `ok` with the
standard `err` object. Usage failures retain exit code 2. No human text is
mixed into stdout. Session output bytes are never logged; non-JSON
`session output` decodes standard base64 and displays UTF-8 lossily, while JSON
preserves the exact `SessionOutputResult`.

`session new` accepts either `--input <text>` or bounded UTF-8 stdin through
`--input-stdin` / `--stdin`, never both. `session input` accepts either
positional text or `--stdin`, never both. Stdin payloads do not appear in argv,
diagnostics, or logs. The CLI validates observation byte/wait bounds and paired
runtime coordinates before dialing. `session wait` and waiting `session output`
use the Rust SDK's dedicated connections and preserve inherited request-origin
markers. `session new --request-timeout-ms <u32>` overrides the response deadline
for that creation request; zero is rejected.

For example, keep untrusted prompt text out of argv by writing it on stdin:

```bash
printf '%s' 'Redacted input.' | pohunek session input s-42 --stdin --json
```

The resulting stdout remains exactly one JSON envelope. The input bytes do not
appear in that envelope, diagnostics, or structured logs.

## Hermes Operator Plugin CLI

The Hermes operator plugin is a local CLI lifecycle, not a daemon public method:
M3 does not change public protocol version `2` or add a Hermes-specific wire
shape. It embeds the plugin assets and generated skill in the `pohunek` binary,
then installs them only into an explicitly selected Hermes profile or custom
absolute home.

```bash
pohunek integration install --agent hermes --hermes-profile default \
  --access-mode manage --allow-host local \
  --tool-timeout-ms 8000 --max-output-bytes 262144 \
  --max-screen-bytes 65536 --max-concurrency 1 --json
pohunek integration doctor --agent hermes --hermes-profile default --json
```

`--hermes-profile default`, a named `--hermes-profile`, and an absolute
`--hermes-home` are explicit target selections; a profile and home cannot be
combined. `status`, `doctor`, `update`, and `uninstall` are Hermes-only and
return `configuration/integration_action_unsupported` for another agent. The
existing daemon-backed Codex/Claude `integration install` behavior is unchanged.

The installation policy is Pohunek-owned, owner-private, and external to the
immutable plugin checksum set. It fixes the absolute `pohunek` executable,
protocol range, access mode, and host allowlist. `read_only` exposes only read
tools, `manage` adds bounded management, and `full` alone adds stop/remove.
Remote calls use the existing direct NetBird transport. The policy is a
delegated-tool guardrail, not an authorization sandbox for a same-user process.

Install and update accept the non-repeatable bounds `--tool-timeout-ms <u32>`,
`--max-output-bytes <u32>`, `--max-screen-bytes <u32>`, and
`--max-concurrency <u8>`. Values must be positive and cannot exceed the policy
ceilings. Install defaults omitted bounds to their ceilings. Update inherits
each omitted bound and replaces each supplied bound; it also always refreshes
the stored protocol range from the updating Pohunek binary so it repairs
protocol drift. Other installed policy fields remain unchanged unless their
existing update flags replace them. Status, doctor, and uninstall do not accept
the bound flags.

The plugin never offers raw attach bytes, arbitrary protocol methods, raw argv,
or force bypasses. It repeats the daemon-authoritative origin denial before a
subprocess for exactly `session.stop`, `session.resume`, `session.remove`,
`session.fork`, `session.resize`, `session.set_metadata`, `session.rename`, and
`session.input`. Exactly three lifecycle reports may target the origin:
`session.report_agent`, `session.release_agent`, and
`session.report_native_id`.

## CLI Notification Surface

The CLI exposes durable notifications through `pohunek notifications`:

- `pohunek notifications list`: list records on one host; `--all-hosts` includes
  local plus reachable daemon hosts from the standalone local-NetBird discovery
  cache. It does not need a local daemon to expand remote targets.
- `pohunek notifications watch`: stream `notification_created`,
  `notification_updated`, and `notification_deleted`; `--all-hosts` opens one
  subscription per reachable host.
- `pohunek notifications read|ack|archive|delete <target>`: update one record.
  Targets accept bare `id` or `host/id`; an explicit target host overrides
  `--host`.
- `pohunek notifications policy get|set`: read policy or toggle one
  provider/kind flag. `set` accepts `--provider default|codex|claude|hermes`,
  `--kind <kind>`, and exactly one of `--enabled` or `--disabled`.
- `pohunek notifications retention prune`: explicitly prune records selected by
  `--status`, `--before`, and `--limit`, with exactly one of `--dry-run` or
  `--apply`. Automatic age-based retention runs independently in the daemon
  from the persisted policy returned by `policy get`.

Commands that support `--all-hosts` render per-host successes and structured
per-host errors. Cross-host notification aggregation is client-side; no central
notification server is introduced.

## Attach Stream

The attach byte stream is part of public protocol v2. It is not an implementation
detail of the CLI.

Sequence:

1. On a normal control connection, send `session.attach` with
   `SessionAttachParams`. Interactive clients should include validated
   `initial_dimensions` when their terminal geometry is known.
2. The daemon returns `SessionAttachResult` with a one-shot `stream_id`.
3. Open a second connection to the same daemon and transport family.
4. Send exactly one newline-delimited attach prelude:

   ```json
   {"attach":"a-1"}
   ```

5. After the prelude newline, the worker applies `initial_dimensions` when
   present and the connection switches to raw bidirectional PTY bytes. The
   daemon first sends one complete ANSI repaint of the current terminal state,
   followed atomically by live PTY output at the repaint watermark. User input
   flows client-to-daemon.
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
- A fresh attach never reconstructs the screen by replaying raw output emitted
  at historical terminal sizes. It starts from the current terminal snapshot,
  then receives live bytes without a gap or overlap.
- `initial_dimensions` is optional for non-terminal clients. Omitting it keeps
  the worker's current geometry while preserving snapshot-first attach.
- Workers negotiated below private worker protocol v3 cannot provide the
  atomic resize-and-snapshot guarantee. The daemon rejects such an attach with
  `runtime/attach_snapshot_unsupported`; restart the session on the upgraded
  worker or fork it into a new session.
- `session.detach` cancels an active stream by `stream_id`; closing the raw
  socket also ends the attach. If the worker ended the raw stream with a typed
  failure, the first detach call after EOF returns that failure in the optional
  `error` field. The result is bounded, short-lived, and consumed once.
- `session.attach` may include `origin_session_id`, `origin_worker_id`, and the
  additive legacy `origin_daemon_id`. New clients read the stable worker id from
  their managed PTY environment. When the session and worker identify the
  target runtime the client is already running inside, the daemon rejects the
  attach with `daemon/attach_self_feedback`; this remains correct after daemon
  replacement.
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
  rendering. An elapsed response deadline maps to `request_timeout`, distinct
  from connection and daemon-discovery failures; the timed-out mutation may
  still have completed remotely.
- `next_request_id(method)`: shared correlation-id generator used by SDK-backed
  clients.
- `discover_hosts()`: local-NetBird peer discovery with default bounded probes.
- `discover_hosts_with_options(options)`: the same discovery with an explicit
  non-zero daemon port, per-probe timeout, overall deadline, and concurrency
  bound. It needs local NetBird state but no local `pohunekd`.
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
  version selected by the first valid response on that connection.
- `Client::selected_version() -> Option<ProtocolVersion>`: returns the fixed
  per-connection selection after the first response.
- `Client::session_screen(SessionScreenParams)`: reads one rendered snapshot on
  the current connection.
- `Client::session_output(SessionOutputParams)`: uses the current connection for
  an immediate read and automatically opens a dedicated connection when
  `wait_ms` is present.
- `Client::session_input(SessionInputParams)`: uses a dedicated connection when
  `wait` is present, budgets the wire timeout as the daemon's overall
  delivery-and-wait deadline plus fixed response headroom, and rejects successful
  responses that omit epoch- and runtime-scoped activity evidence.
- `Client::session_wait(SessionWaitParams)`: automatically opens a dedicated
  connection for the bounded long poll.
- `Client::session_resume`, `session_resize`, and `session_set_metadata`: typed
  lifecycle helpers used by automation clients.
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
  `connectTimeoutMs` and `requestTimeoutMs`, both defaulting to 5000 ms, plus an
  optional validated `origin: {sessionId, daemonId}` pair.
- `RequestOrigin` and `resolveRequestOrigin`: explicit browser-safe origin
  configuration and atomic identifier validation. Browser and Bun/Node defaults
  are absent; the SDK never reads `process.env`.
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
  map. A configured origin is added to the wire request.
- `client.sessionInput(params)`: uses a dedicated connection when `wait` is
  present, budgets the wire timeout as the daemon's overall delivery-and-wait
  deadline plus fixed response headroom, and rejects successful responses that
  omit epoch- and runtime-scoped activity evidence.
- `client.handshake()`: calls `daemon.health` and enforces strict protocol
  version equality.
- `client.request(request)`: validates the optional atomic wire origin, applies
  the client origin when configured, sends one raw request envelope, and returns
  the `ok` payload.
- `client.subscribe(request)`: applies the same configured origin, consumes the
  control connection after the subscribe ack, and returns `Subscription`.
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
  `host_unreachable`, `remote_daemon_unavailable`, `request_timeout`, `io_error`,
  `json_error`, `session_input_wait_contract_mismatch`, and `version_mismatch`.
- `ClientError.toProtocolError()` returns the structured `ProtocolError` for
  CLI/API rendering, and `recoverHint()` returns the optional recovery text.

The `@pohunek/backend` package (renamed from `@pohunek/relay`) exports the
transport-core server used by `WsTransport`:
`startRelay({bindHost, port, targets, allowLoopbackBind?})`,
`validateRelayBindAddr`, `isNetbirdIp`, `RelayBindAddrError`, and the
`DaemonTarget`/`RelayHandle` types. The package rename and its added host
discovery, `/api/hosts`, and SPA composition do not change the WebSocket relay
framing contract documented above: it remains a transparent 1:1 tunnel.
