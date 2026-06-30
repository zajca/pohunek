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
| `session.new` | `SessionNewParams` | `SessionNewResult` | Starts an agent PTY session. |
| `session.list` | `SessionListParams` or `null` | `Vec<SessionInfo>` | Lists sessions; filters use AND semantics. |
| `session.inspect` | `SessionId` | `SessionInfo` | `SessionId` is a JSON string, e.g. `"s-1"`. |
| `session.stop` | `SessionId` | `SessionStopResult` | Stops a live session (the entry stays in `list`). |
| `session.remove` | `SessionId` | `SessionRemoveResult` | Evicts a session from the registry, stopping it first if still live. Unknown id is `session_not_found`. |
| `session.attach` | `SessionAttachParams` | `SessionAttachResult` | Mints a one-shot attach stream id. |
| `session.detach` | `SessionDetachParams` | `SessionDetachResult` | Cancels an active attach stream. Unknown streams return `detached: false`. |
| `session.resize` | `SessionResizeParams` | `SessionResizeResult` | Resizes the PTY on the control connection. |
| `session.input` | `SessionInputParams` | `SessionInputResult` | Injects text using agent-specific input framing. |
| `session.report_native_id` | `SessionReportNativeIdParams` | `SessionReportNativeIdResult` | Hook callback for resume metadata. |
| `session.set_metadata` | `SessionSetMetadataParams` | `SessionSetMetadataResult` | Merges owner-controlled metadata. Values must not contain secrets. |
| `session.rename` | `SessionRenameParams` | `SessionRenameResult` | Sets or clears a session's owner display name (`name: null` clears). Cosmetic; the daemon trims it and rejects a control character or over-long name. |
| `subscribe` | `null` | `{subscribed: true}` then event stream | Consumes the connection into a one-way event stream. |
| `integration.install` | `IntegrationInstallParams` or `null` | `IntegrationInstallResult` | Installs agent hooks for native session id capture. |
| `assistant.materialize` | `AssistantMaterializeParams` | `AssistantMaterializeResult` | Materializes the assistant knowledge bundle on the daemon host. |
| `project.list` | `ProjectListParams` or `null` | `Vec<ProjectInfo>` | Lists known projects on the target host. |
| `project.add` | `ProjectAddParams` | `ProjectInfo` | Registers a host-local git project path. |
| `project.show` | `ProjectShowParams` | `ProjectShowResult` | Shows a project plus live worktree state. |
| `project.rename` | `ProjectRenameParams` | `ProjectInfo` | Sets a custom display label. |
| `project.remove` | `ProjectRemoveParams` | `ProjectRemoveResult` | Removes a project record and optionally owned worktrees. |
| `project.prompt` | `ProjectPromptParams` | `ProjectPromptResult` | Resolves a prompt template without rendering it. |
| `project.action` | `ProjectActionParams` | `ProjectActionResult` | Resolves one action recipe plus prompt content. |
| `project.actions` | `ProjectActionsParams` | `ProjectActionsResult` | Lists available project actions after layer shadowing. |

`status` exists as a method constant in `crates/protocol` but is not a supported
daemon method in this API version. It returns `daemon/method_not_found`.

## Core Payloads

This section names the high-value fields clients commonly branch on. The full
wire shapes are the exported `crates/protocol` structs.

### `SessionInfo`

Important fields:

- `id`: stable session id.
- `name`: optional owner-set display name; absent means the session is shown by
  its id. Set at `session.new` and changed via `session.rename`.
- `agent`: profile name.
- `agent_base`: `shell`, `codex`, or `claude`.
- `cwd`: host-local working directory.
- `pid`: root process id.
- `cols`, `rows`: current PTY size.
- `state`: `starting`, `running`, `stopped`, `done`, or `failed`.
- `state_source`: `osc_title`, `osc_progress`, `screen`, or `process`.
- `activity`: optional `working`, `blocked`, or `idle`.
- `native_session_id` / `native_session_path`: optional agent resume binding.
- `project_id`, `project_label`, `repo`, `branch`, `worktree_path`: optional git
  and project context.
- `warnings`: non-fatal worktree setup warnings.
- `metadata`: owner-controlled strings; must not contain secrets.
- `created_at`, `updated_at`: RFC3339 timestamps.
- `exit_code`: optional process exit code.

### Session and Project Filters

`session.list` and `project.list` filters are exact-match predicates combined
with AND semantics. They are tagged objects, for example:

```json
{
  "filters": [
    {"key":"state","value":"running"},
    {"key":"agent","value":"codex"}
  ]
}
```

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
| `runtime` | `agent_binary_missing`, `agent_profile_not_found`, `invalid_profile`, `agent_not_resumable`, `invalid_session_ref`, `no_capable_agent`, `bundle_unavailable`, `assistant_bundle_mismatch`, `materialization_failed`, `agent_cannot_read_bundle`, `session_not_found`, `session_not_running`, `session_exit_timeout`, `attach_not_found`, `attach_expired`, `pty_alloc_failed`, `spawn_failed`, `pty_error`, `io_error`, `project_store_error`, `project_detect_failed`, `not_a_git_repo`, `project_not_found`, `project_ambiguous`, `prompt_not_found`, `template_not_found`, `action_not_found`, `invalid_name`, `invalid_template`, `invalid_action`, `path_escape`, `config_read_failed`, `agent_not_installable`, `agent_config_dir_missing`, `integration_settings_invalid`, `integration_io_failed`, `worktree_store_error`, `worktree_path_conflict`, `invalid_base_branch`, `worktree_branch_in_use`, `worktree_add_failed`, `invalid_branch`, `invalid_branch_slug` |

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
| `session_created` | `{session: SessionInfo}` | A session was created. |
| `session_updated` | `{session: SessionInfo}` | Session metadata, state, resize, resume binding, or terminal state changed. |
| `session_stopped` | `{session: SessionInfo}` | A user-requested stop completed. |
| `session_removed` | `{session: SessionInfo}` | A session was evicted from the registry; clients drop it from their view. |
| `agent_state` | `{session_id: SessionId, activity: AgentActivity, source: StateSource}` | Agent activity changed. |
| `attach_opened` | `{session_id: SessionId, stream_id: string}` | A pending attach token was redeemed and a raw stream opened. |
| `attach_closed` | `{session_id: SessionId, stream_id: string}` | A raw attach stream ended or was detached. |

Subscription connections ignore further client input after the ack. A slow
subscriber may miss older events if the daemon's internal event channel lags; it
should reconcile by calling `session.list` or `session.inspect`.

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
- Raw and attach helpers: `connect_raw*` and `attach_raw*`.

Connection APIs:

- `Client::connect(host, socket_path)`: `""` and `"local"` use the Unix socket;
  any other host is resolved through NetBird and dialed over TCP.
- `Client::connect_local(socket_path)`: direct Unix socket.
- `Client::connect_tcp_addr(host, addr)`: direct TCP with host context preserved
  for remote errors.
- `*_with_options` variants accept `ClientOptions`.

Request APIs:

- `Client::request(&Request) -> serde_json::Value`: sends one request and returns
  the `ok` payload.
- `Client::subscribe(&Request) -> Subscription`: consumes the client connection
  after a subscribe ack.
- `Subscription::next_line() -> Option<String>`: returns raw event JSON lines.

SDK error mapping preserves daemon protocol errors and adds host/transport
context for local and remote failures. Use `ClientError::to_protocol_error()` to
render SDK failures in the same envelope taxonomy as daemon errors.
