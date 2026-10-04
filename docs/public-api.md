# Pohunek Public API

This document is the public API contract for the daemon control protocol and the
Rust SDK surface that speaks it.

Status: versioned public API, pre-1.0 stability. Breaking changes are allowed
before 1.0, but they must be reflected in the protocol version and this document.

Source of truth:

- Wire types and method constants: `crates/protocol`
- Rust SDK transport API: `crates/client`
- Assistant launch orchestration and host connection types
  (`HostConfig`, `ConnectionOptions`, `connect_client`, `AssistantError`):
  `crates/assistant`
- Daemon dispatch behavior: `crates/daemon/src/api`

## Status: Shipped v4, Implemented Relay Foundation, and Deferred Relay Evolution

### Shipped now: protocol v4 owner paths

Protocol v4 is the implemented contract. Local clients connect through the
owner-only Unix socket, direct remote clients connect through a configured
overlay such as NetBird, and browser clients can use a transparent WebSocket
relay speaking the transport contract described below (the web control center,
an external `zajca/pohunek-work` client, ships its own). All three routes retain
the existing owner trust domain. Such a relay maps one WebSocket to one
daemon connection; it is not a team service, authentication authority, state
aggregator, or public `pohunek-relayd` implementation.

Version 4 also ships host-local stable identity and safe governance inspection.
It does not ship a relay connection, relay-local transport, enrollment or owner
mutation RPC, team WebUI, share API, or public transfer API. Existing local,
direct-overlay and owner-path browser relay paths remain owner-only
and unchanged.

### Implemented relay foundation outside protocol v4

`pohunek-relayd` is an independently configured HTTPS authority, backed by
PostgreSQL. It is not a `pohunekd` protocol-v4 endpoint and it does not add a
host link, routing, attach, or team WebUI. The implemented bounded surface
includes liveness/readiness, generic OIDC browser Authorization Code with PKCE
and device authorization, account and credential lifecycle, account linking, and
service-account credential lifecycle. It also has protected stopped-lifecycle
`migrate`, `bootstrap`, `provision`, and recovery commands.

The native `pohunek relay` commands use a separate HTTPS-only client and the OS
keyring for `login`, `status`, `logout`, credential `rotate`, and interrupted
rotation recovery. They bind a credential to one exact HTTPS origin. This is a
foundation surface, not a complete team-management or host-control API. The
account-linking routes below have no `pohunek relay` subcommand; they are
HTTPS-only.

### Relay account-linking surface

Account linking proves control of one additional stable OIDC identity and
attaches it to an existing relay account. A relay identity is exactly the issuer
plus the immutable subject. Email, display name, Google `hd`, GitHub login, and
every other provider profile attribute are attributes, never linking inputs, and
matching email strings never link accounts.

Wire types are exported by `crates/relay-protocol`, and JSON field names are the
Serde field names of those structs. Every route is served with `no-store`, sits
behind the existing relay authentication boundary, and accepts either a browser
session cookie or a bearer credential in one request, never both. A browser
mutation additionally requires the exact configured `Origin` and an
`x-pohunek-csrf` header matching the CSRF cookie.

| Route | Request | `200` response | Notes |
|---|---|---|---|
| `POST /v1/account/links/browser/start` | `AccountLinkRequest` | `AccountLinkBrowserStart` | Browser session only; a bearer caller is rejected. Returns `{link, authorization_url}` and sets the one-use host-only `__Host-pohunek-relay-login` binding cookie, so the authorization URL alone cannot complete the transaction. |
| `POST /v1/account/links/device/start` | `AccountLinkRequest` | `AccountLinkDeviceStart` | Bearer credential only; a browser caller is rejected. Returns `{link, authorization}` where `authorization` carries the RFC 8628 `verification_uri`, optional `verification_uri_complete`, `user_code`, `expires_at`, `interval_seconds`, and the one-use `poll_secret`. The poll value is delivered once and is stored only as a keyed digest. |
| `POST /v1/account/links/device/poll` | `AccountLinkPollRequest` plus the `x-pohunek-device-secret` header | `AccountLinkPollResult` | Polled by `link_id` by the same bearer credential that started the transaction; the possession value travels in the header, never in the URL. The `status` tag is `pending` or `slow_down` with `retry_after_seconds`, `complete` with `{link, identity}`, `denied`, `expired`, or `cancelled`. |
| `GET /v1/account/links` | `PageRequest` query (`after`, `limit`) | `AccountLinkPage` | Read-only history for the calling account only, ordered by `link_id` ascending, with `next_cursor` for continuation. `limit` above 128 is rejected. No CSRF is required. |
| `POST /v1/account/links/{link_id}/cancel` | `CancelAccountLinkRequest` | `AccountLinkRecord` | Cancels one pending transaction owned by this account and closes its unconsumed provider row. An already cancelled transaction returns its current record, so a retry after a lost response is safe. |
| `POST /v1/account/identities/{identity_id}/unlink` | `UnlinkIdentityRequest` | `IdentityRemoved` | Removes one active linked identity and returns `{identity_id, principal_id, removed_at, account_link_generation, revoked_credentials, revoked_sessions}`. An exact retry of the same `idempotency` coordinate returns the committed result instead of removing a second identity. |

`AccountLinkRecord` is the safe revisioned view of one transaction: `link_id`,
`principal_id`, `channel` (`browser` or `device`), `state` (`pending`,
`completed`, `cancelled`, `expired`, or `failed`), `source_identity_id`,
`linked_identity_id`, `revision`, `account_link_generation`, `created_at`,
`expires_at`, and `completed_at`. Possession digests, provider tokens, and
verifier material never appear in it, and `IdentityRecord` carries only
`identity_id`, `issuer`, and `subject`.

A browser link completes through the existing `GET /v1/auth/oidc/callback`. That
callback must present both the one-use login binding cookie and the caller's
current session cookie; it consumes the `BrowserLogin` row before code exchange,
redirects to `account/ready`, and issues no new session cookie for a link. A
device link completes on the poll that returns `complete`. Both channels use the
same PKCE and device validation rules as login, and a transaction is completable
only through the channel that created it.

Every transaction is bound at creation to the initiating principal, the source
identity, the exact authentication row and its generation, the issuer, client
and audience, a keyed digest of its one-use possession value, the digest key id,
the account-link generation, the recovery generation, and a bounded expiry. A
completion re-checks all of those against current durable state, so a rotated or
revoked source credential, a replaced browser session, an advanced account-link
generation, a changed recovery generation, or a cross-principal caller fails the
transaction instead of linking.

PostgreSQL holds the durable guarantees rather than application code alone: one
pending transaction per account, one created identity per completed transaction,
uniqueness of `(issuer, subject)` across identities that are not removed,
monotonic `account_link_generation`, immutable transaction provenance, and a
terminal state that cannot be revived or rewritten without advancing `revision`.
A removed identity keeps its row so credential and browser-session provenance
stays resolvable.

A completed link advances the account's `account_link_generation`. An unlink
revokes that identity's credentials and browser sessions, cancels every pending
link transaction for the account, and advances both the account-link generation
and the principal generation in the same transaction as the audit record; an
audit or database failure fails closed and closes active access rather than
acknowledging an authority change. An account never removes its last identity.

An unlink removes that identity's authority over the account; it is not a ban on
the identity. The issuer and subject can authenticate again afterwards, and
teamless authentication provisions a brand-new principal with no membership, so
the returning identity authorizes no team resource and has no path back to the
account it was removed from. Barring an identity from the relay is a separate
administrative decision, not a consequence of unlinking.
Restore quarantine cancels every pending link transaction and blocks link
changes while the recovery generation is stale.

Failures use the relay's HTTP error contract with a stable `code`:

| Status | `code` | Cause |
|---|---|---|
| `400` | `invalid_request` | A transaction presented through the wrong channel, `Authorization` mixed with a cookie or `Origin`, a malformed body, or a page limit above the bound. |
| `401` | `authentication_required` | No current credential or session, a failed CSRF or `Origin` check, or a poll possession value that does not match the transaction. |
| `403` | `forbidden` | An actor with no stable OIDC identity, such as a service account, or a completion that does not match the initiating account. |
| `404` | `not_found` | No link transaction or active identity with that coordinate belongs to this account. |
| `409` | `link_pending` | The account already has an open link transaction. |
| `409` | `link_replayed` | The transaction, callback, or identity proof was already consumed. |
| `409` | `link_cancelled` | The transaction was cancelled before it could be proven. |
| `409` | `link_self` | The proven identity is already linked to this account. |
| `409` | `link_collision` | The proven identity belongs to another account. |
| `409` | `last_identity` | Removing the identity would leave the account unable to authenticate. |
| `409` | `state_conflict` | A link coordinate is no longer current, or a retry coordinate conflicts with a different request. |
| `410` | `link_expired` | The transaction passed its bounded expiry. |
| `503` | `unavailable` | Recovery quarantine, a durable failure, or a configured issuer with no usable device endpoint. |

### Deferred optional team relay

The [team relay RFC](design/team-relay-control-plane-rfc.md) defines the next
extension. Its host-link and team API are not part of the shipped protocol-v4
contract. The extension keeps local Unix and direct overlay owner
paths unchanged and adds a separate Rust `pohunek-relayd` authority. A host will
initiate an authenticated userspace WireGuard tunnel and every control and
attach TCP stream; the public relay will never dial the host.

The normative future contract is in the RFC's identity/evidence, recovery,
host-link scheduling, resource, snapshot, catalog, audit, failure, and
ownership sections. It does
not add an API to this document before its owning issues ship. The coordinated
protocol-v4 cutover in [#70](https://github.com/zajca/pohunek/issues/70) will
add the authenticated host link, `HostShare` coordinates, immutable session
origin, atomic host snapshots, and host-initiated attach streams. There will be
no v4 relay compatibility shim and no change to the direct-owner trust domain.
The dependency path after the completed reduced [#85](https://github.com/zajca/pohunek/issues/85) is
[#107](https://github.com/zajca/pohunek/issues/107) and
[#108](https://github.com/zajca/pohunek/issues/108) →
[#92](https://github.com/zajca/pohunek/issues/92) → complete
[#72](https://github.com/zajca/pohunek/issues/72), followed by #70, #82, #83,
#84, and [#71](https://github.com/zajca/pohunek/issues/71).

The external owner-mode transparent browser transport (the web control center
backend in `zajca/pohunek-work`) remains supported after [#86](https://github.com/zajca/pohunek/issues/86) adds a
separate typed team-relay client. The Rust relay has no local mode. The two web
surfaces may reuse presentation components, but they use explicit origins,
transports, credentials, and state with no cross-mode fallback. Existing names,
including `WsTransport.relay`, describe shipped SDK API and do not imply that
the accepted team relay has shipped.

## Compatibility Model

The current public protocol version is `4` (`PROTOCOL_VERSION`), and this build
supports the inclusive range `4..=4` (`SUPPORTED_PROTOCOL_VERSIONS`). Requests
carry `v: {minimum, maximum}`. The first valid response selects the highest
overlapping version as an integer `v`, and that selection is fixed for the
lifetime of the connection. Subscription events use the same selected version.
A non-overlapping range returns `daemon/version_mismatch`.

Protocol v3 is the coordinated overlay-routing boundary. It changes
`HostRecord.address` to an optional IP-only value and adds required `port` and
`overlay` fields plus optional `peer_id`. Every daemon and bundled client must
be upgraded together; no v2 compatibility shim is provided.

The former exact integer request envelope is deliberately rejected. The
historical protocol-v2 transition was the one-time move from integer-v1 to
range negotiation; it provided no v1 envelope or notification-policy shim.
The protocol-v3 transition likewise required a coordinated upgrade from v2.

Protocol v4 renames the public worker instance identifier from `runtime_id` to
`worker_instance_id` on `SessionRuntimeIdentity` (and therefore the flat
`runtime_id` of the output, screen and read results and the `runtime` objects of
their params), `SessionRuntime`, `RuntimeInventoryEntry`,
`SessionReportNativeIdParams`, and `SessionNativeRecoveredEvent` (including
`previous_runtime_id`, now `previous_worker_instance_id`). `RuntimeId` keeps its
agent-runtime meaning. A v3 client that sends its range `3..=3` to a v4 daemon
(or the reverse) receives `daemon/version_mismatch` before any method runs, so
it never sees a renamed field; there is no `runtime_id` alias on the public wire.
Every daemon, CLI, SDK, managed hook and Hermes plugin must be upgraded
together. Notification hooks deliver through the worker socket when
`POHUNEK_WORKER_SOCKET_PATH` is set and fall back to the daemon socket otherwise;
the worker's `notification_create` hook request is additive to the private
worker protocol, so a worker that predates it simply refuses it. Managed hook assets carry `POHUNEK_INTEGRATION_VERSION=10`; an asset of
an earlier version still sends the old key (and an earlier notification hook still
sends a bare integer `v` instead of the `{minimum, maximum}` range, so the daemon
rejects its notifications), so `integration.status` reports it
`outdated` and `integration.doctor` as an asset finding until it is reinstalled.

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
appropriate negotiated-version treatment. `RuntimeRef` values and provider-policy map
keys are deliberately open value namespaces, not open object shapes.

Non-additive wire changes require a protocol version bump. Examples: changing a
required field name or type, removing a field, changing enum string values,
changing an existing method's result shape, or changing attach stream framing.

## Transports

The daemon exposes the same protocol on two transports:

| Transport | Endpoint | Security boundary |
|---|---|---|
| Local | Unix socket at the configured runtime path | Owner-only socket directory and mode |
| Remote | One TCP listener per configured overlay, bound to that provider's validated local member address and port | Overlay reachability and provider policy; NetBird/WireGuard is the default production provider |

The local runtime path is a shared host/client contract, not a wire-protocol
field. A valid absolute `XDG_RUNTIME_DIR` resolves the application root to
`$XDG_RUNTIME_DIR/pohunek` on Linux and macOS. Linux fails fast when it is
absent. macOS instead uses `/private/tmp/pohunek-<effective-uid>` when the
variable is absent; `TMPDIR` does not select this default. An explicit empty,
relative, or otherwise malformed value is rejected rather than treated as
absent. The root must be an effective-UID-owned real directory with exact mode
`0700`; the daemon socket is `<runtime-root>/daemon.sock` with mode `0600`.
Unsafe symlinks, foreign entries, wrong types or modes, and encoded socket paths
that exceed the native Unix-socket limit are errors and are never repaired,
deleted, or truncated implicitly.

The Rust daemon, CLI, workers, and hooks use this shared resolver, and the
TypeScript SDK (`@pohunek/sdk`) implements the same contract: both are driven by the cases in
`crates/paths/fixtures/runtime-paths.json`.

The JSON control stream is newline-delimited UTF-8 JSON. One JSON value is sent
per line. The current daemon and Rust SDK cap control lines at 1 MiB.

Raw terminal bytes are never multiplexed onto a JSON control connection. Attach
uses a separate connection described in "Attach Stream".

The TypeScript SDK also supports a transparent WebSocket relay transport
(`WsTransport`) for browser and Bun/Node clients that cannot dial Unix sockets
or daemon TCP directly. Browser code imports the browser-safe
`@pohunek/sdk/browser` entry; the root `@pohunek/sdk` entry additionally
exposes Bun/Node socket transports. A relay is not a daemon protocol endpoint
and does not aggregate state. It is a pure one-WebSocket-to-one-daemon-connection
tunnel:

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
- The `<host>` URL segment is resolved only through the relay's own host
  catalog. Unknown, stale, or unreachable hosts are rejected during upgrade.
- Bind policy is the relay host's concern. A production relay should bind
  fail-closed to the owner overlay (for NetBird, a CGNAT address in
  `100.64.0.0/10`); wildcard addresses such as `0.0.0.0` and `::` are never
  valid.

This transparent WebSocket framing contract is pre-1.0 transport
infrastructure. It remains the owner browser path alongside the future
team surface; it is not the accepted public team-relay contract.
`@pohunek/testkit/bun-relay` ships `startTestRelay`, a loopback-only,
Bun-only implementation of it used by the SDK transport tests.

## Envelopes

### Request

```json
{"v":{"minimum":4,"maximum":4},"id":"req-7f3","method":"session.list","params":{}}
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
the pair; the Rust SDK propagates inherited origin (unless its options select
`OriginSource::Omitted`) and the TypeScript SDK
propagates an explicitly configured `ConnectOptions.origin` to normal,
subscription, and dedicated connections. When both markers identify the target
as the caller's own origin session, the daemon returns
`runtime/plugin_self_target_denied` for exactly `session.stop`,
`session.resume`, `session.remove`, `session.remove_accepting_unconfirmed`,
`session.fork`, `session.resize`, `session.set_metadata`, `session.rename`, and
`session.input`. Read-only methods,
including observation, remain available. The lifecycle reports
`session.report_agent`, `session.release_agent`, and `session.report_native_id`
are explicitly allowed because hooks must report their own session; the public
native-id report is the necessary local fallback when the owner-private worker
claim cannot be delivered. The owner-private worker claim is additionally bound
to kernel peer identity — the worker accepts a report only from the process it
names or from a descendant of that process, and never from a request field —
while this public fallback has no peer binding and rests on its runtime,
ordering, expiry, and provider rules alone. This is a narrow server-side confused-deputy guard
inside the existing single-operator trust boundary, not per-session
authentication or a broader mutation policy.

### Response

Successful response:

```json
{"v":4,"id":"req-7f3","ok":{"status":"ok"}}
```

Error response:

```json
{
  "v": 4,
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
{"v":4,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title"}
```

Event payload fields are flattened at the top level beside `v`, `event`, and the
optional `id`.

## Public Methods

All params and result type names below refer to structs exported by
`crates/protocol`. JSON field names are the Serde field names of those structs.

| Method | Params | `ok` result | Notes |
|---|---|---|---|
| `daemon.health` | `null` | `{status, daemon_version, protocol_version}` | Liveness and version probe. |
| `daemon.doctor` | `null` | `DaemonDoctorResult` | Runs daemon-local checks. Non-null params are `daemon/bad_request`. Each check is `{name, status, detail}`: `name` is the stable code and `detail` carries the remediation. The list is platform specific: Linux reports `bin:git`, `bin:codex`, `bin:claude`, the socket, state and log directory writability checks, `netbird_cli` and `schema_version`; macOS adds `runtime_dir_private`, `socket_path_length`, `filesystem_access`, `worker_executable`, `login_shell`, `launchd_domain`, `desktop_notifications` and `keychain`. Hook interpreter readiness (`python3`) is reported by `integration.doctor`, not here. `overall` is `fail` only for required failures. |
| `host.inspect` | `null` | `HostCapabilities` | Live capability snapshot for the daemon's host. |
| `host.discover` | `HostDiscoverParams` or `null` | `Vec<HostRecord>` | Enumerates peers from configured overlay transports and classifies daemon reachability. |
| `host.governance.inspect` | `null` | `HostGovernanceStatus` | Owner-safe, read-only snapshot of the daemon's stable host identity and local governance state. Explicit `null` and omitted params both succeed; every non-null JSON value returns `daemon/bad_request`. The method never changes enrollment, ownership, or sessions. On unavailable governance it returns the fixed redacted `daemon/host_governance_unavailable` error; reload or restart the daemon, then retry. |
| `session.new` | `SessionNewParams` | `SessionNewResult` | Starts an agent PTY session. Bare and profile-based Hermes launches first require an executable whose isolated, bounded `--version` probe matches the pinned supported release; failure returns payload-free `agent_runtime_unsupported` before session, worker, or worktree creation. While startup found legacy resume bindings with no logical record and no migration manifest (listed in `session.runtime_inventory` as `orphaned`, `migration_manifest_missing`), it fails with `migration_manifest_missing` before anything is written, because a first logical record would make a later manifest unimportable; run `pohunek migration preflight` against the legacy daemon, and the next daemon start that imports the manifest lifts the refusal. A session whose worker socket could never be bound (the published `control.sock` path or the longer staged bind path beside it exceeds the platform `sun_path` limit) fails with configuration-class `worker_socket_path_invalid`, naming the path and the limit, before anything is written or launched; `session.fork` applies the same check. A worker job that ends before accepting connections fails the call with `worker_exited_before_ready` as soon as the service manager shows the job ended, carrying the worker's exit status where the supervisor retains it, instead of waiting for the worker connect deadline (`worker_connect_failed`). A create that fails after binding a worktree is compensated once its runtime is proven ended (checkout and binding, then the durable record); a compensation that cannot finish (for example a checkout locked with `git worktree lock`) still fails the call with its original error, keeps the record, and lists the session as `reconnecting` with reason `create_compensation_pending` while the daemon retries the compensation with the supervision backoff, until the session leaves with `session_removed`. Optional `metadata` is written atomically with the session (see the `metadata` field note under `SessionInfo` below); the CLI exposes it as repeatable `--meta key=value`. |
| `session.list` | `SessionListParams` or `null` | `Vec<SessionInfo>` | Lists sessions; filters use AND semantics. |
| `session.inspect` | `SessionId` | `SessionInfo` | `SessionId` is a JSON string, e.g. `"s-1"`. |
| `session.stop` | `SessionId` | `SessionStopResult` | Stops a live session (the entry stays in `list`). |
| `session.resume` | `SessionId` | `SessionResumeResult` | Explicitly recovers a terminal or lost logical session from captured native recovery metadata, reusing the logical session id but starting a new worker generation (a new native service job) with a new runtime id. The previous generation must be proven ended first; two generations of one session are never live at once. Hermes recovery revalidates the frozen executable against the pinned release before any recovery write or worker launch and returns payload-free `agent_runtime_unsupported` when unavailable or incompatible. Live, reconnecting, conflicting, or incompatible runtimes are rejected; sessions without native metadata return `not_resumable` or `agent_not_resumable`; an assigned reference whose conversation cannot be found returns `agent_native_reference_missing` before a worker launches. Daemon restart never calls this method automatically. |
| `session.fork` | `SessionForkParams` | `SessionForkResult` | Forks a native agent conversation into a new pohunek session id and PTY, using the source session's cwd/worktree for `cwd_mode: "same"`. Live sources are allowed. Unknown ids return `session_not_found`; external sessions return `session_external_read_only`; sources without launch-agent native metadata return `not_resumable` or `agent_not_resumable`; an assigned reference whose conversation cannot be found returns `agent_native_reference_missing` before anything launches; Codex- and Hermes-backed sessions return `agent_fork_unsupported`; unmigrated legacy resume bindings return `migration_manifest_missing` as for `session.new`. A successful fork emits `session_created`. |
| `session.remove` | `SessionId` | `SessionRemoveResult` | Evicts a session from the registry, stopping it first if still live. Unknown id is `session_not_found`. A `reconnecting` or `incompatible` runtime, or a `conflict` whose reason is `runtime_supervision_ambiguous`, is retired through the service manager by the exact worker generation its record names, and every worker the session's journals record for that generation must then be gone, before the record is deleted. A record in one of those states that names no generation, or a `conflict` for any other reason, fails with `session_runtime_conflict`, `session_runtime_reconnecting`, or `worker_protocol_incompatible`; a retirement the service manager cannot complete fails with `runtime_supervision_unavailable`; a still-running worker journaled under another generation fails with `runtime_identity_mismatch`; unreadable session journals or a journaled worker still running after the retirement fail with `runtime_supervision_ambiguous`. The record is kept in every refused case. Every removal then sweeps the processes carrying the session's runtime ownership markers (every runtime its record or a journal of its generation names); a sweep that cannot confirm every marked process exited fails with `runtime_supervision_ambiguous` after the removal intent was recorded, keeping the session listed so a retried removal, or the next daemon start, finishes it. When same-user processes whose environment cannot be read are the only obstacle, the error `msg` lists them (`pid`, start identity, command name; at most eight, then `and N more`) and `recover` tells the operator to inspect and end the ones belonging to the session and retry; no environment values are included. It always refuses on an unconfirmed sweep; `session.remove_accepting_unconfirmed` is the consent variant. Worktree cleanup is best-effort: the result reports `worktrees_removed` for checkouts that are gone and `worktrees_failed` for an owned checkout whose `git worktree remove` failed, whose binding is dropped regardless, so the leftover directory needs manual cleanup. This is an explicit operator removal and is deliberately not subject to the retention sweep's unsaved-work hold. |
| `session.remove_accepting_unconfirmed` | `SessionId` | `SessionRemoveResult` | Removes a session exactly like `session.remove`, except that when the marker sweep of its runtimes is unconfirmed solely because same-user processes with unreadable environments may belong to them, it proceeds instead of refusing with `runtime_supervision_ambiguous`. The daemon never signals those processes; it logs each accepted process at `warn` once the sweep lets the removal proceed, before any cleanup, and lists the set in `accepted_unconfirmed_processes` (`pid`, `start_identity`, optional `command`; the field is omitted when empty). The list is deduplicated and capped at 64 candidates, checked before anything is deleted, and each `command` is a kernel command name with every character outside ASCII letters, digits, space and `._-+:@/` escaped as `\u{..}` and long names shortened with `...`, so it is safe to print. Consent covers this one call only: nothing stores it, a retried removal needs it again, and the reconciliation finalizer and the retention sweep never have it. Every other unconfirmed reason (a signalled process still running, a sweep error, a missing supervision configuration) and every other refusal of `session.remove` still applies with the same error codes. A process among the accepted ones that does carry the runtime marker keeps running unsupervised after the worktree, logs and record are deleted. A daemon without this method answers `method_not_found`. |
| `session.runtime_inventory` | `null` | `RuntimeInventoryResult` | Returns the durable-worker runtime inventory captured at startup reconciliation: one `RuntimeInventoryEntry` per discovered worker with its runtime slot, claimed session id, worker/runtime ids, and classification (`managed`, `orphaned`, `conflict`, `incompatible`, or `identity_mismatch`). Read-only operator diagnostic; it never mutates or kills a worker. |
| `session.attach` | `SessionAttachParams` | `SessionAttachResult` | Mints a one-shot attach stream id. |
| `session.detach` | `SessionDetachParams` | `SessionDetachResult` | Cancels an active attach stream. After a worker stream failure, the first call returns its optional typed `error` and consumes that short-lived result; unknown or already-consumed streams return `detached: false` without `error`. |
| `session.resize` | `SessionResizeParams` | `SessionResizeResult` | Resizes the PTY on the control connection. |
| `session.input` | `SessionInputParams` | `SessionInputResult` | Injects text using agent-specific input framing. Hermes accepts at most `MAX_SESSION_INPUT_BYTES` UTF-8 bytes, permits LF and tab but rejects other C0/C1 controls without rewriting them, and returns `session_input_blocked` for fire-and-forget input while approval-visible activity is blocked. Unsafe or oversized Hermes text returns `session_input_rejected`. Every per-session input holds one gate through its complete framing transaction, preventing waited/waited and waited/fire-and-forget interleaving. Every worker plan contains a body fragment with the provider `delay_after_ms` and a separate submit fragment; body and Enter are never merged into one paste burst. With `wait`, every Rust/TypeScript typed call path, including generic `Client.call`, routes through the dedicated helper, normalizes absent `until` to `[]` before wire validation, and rejects `timeout_ms` outside `1..=8000` before transport. The daemon deduplicates targets, acquires a waiter permit, starts the overall deadline before the input gate, and rejects blocked activity with `session_agent_blocked` both before the gate and at the causal boundary, independently of provider fire-and-forget policy. A worker-owned submit delay cannot be activity-revalidated and already-written text cannot be safely retracted from an arbitrary TUI, so waited input rejects nonzero-delay framing with `session_input_wait_unsupported` before writing bytes. Zero-delay framing acquires the exclusive worker write reservation before capturing runtime-scoped activity revision evidence immediately before send. Timeout or shutdown before send cancels the reservation and writes no bytes. After send begins, the exchange is cancellation-shielded and retains the session input gate until the late ACK is consumed. Matching evidence above that lower bound and observed through the fixed deadline succeeds, including evidence between PTY plan flush and worker ACK. After the plan is sent delivery outcome may be unknown, so callers inspect the session and do not retry blindly. Bounded evidence history retains the maximum wait window, so a later same-activity report cannot erase valid pre-deadline evidence. The result includes `activity`, `activity_source`, `runtime`, `activity_epoch`, and decimal-string `activity_revision`; SDK helpers require all five and return `session_input_wait_contract_mismatch` when a same-version daemon ignores `wait` or omits evidence. Clients deduplicate by `(activity_epoch, runtime, activity_revision)`. Runtime exit returns `session_not_running`; replacement returns `session_runtime_changed`; external sessions return `session_external_read_only`; shutdown cancels delivery or waiting. The dedicated connection adds fixed response headroom beyond the overall deadline. |
| `session.screen` | `SessionScreenParams` | `SessionScreenResult` | Reads one bounded, rendered, runtime-bound terminal snapshot without acquiring attach ownership or resizing the terminal. |
| `session.detection` | `SessionDetectionParams` | `SessionDetectionResult` | Read-only detector diagnostic. Returns the complete supported region-kind set and current text previews for regions required by the session's active manifest. |
| `session.read` | `SessionReadParams` | `SessionReadResult` | Reads bounded text from the current rendered terminal without attaching. `lines` accepts `1..=1,000`, defaults to that ceiling, and keeps the newest rendered rows. The complete serialized result is capped by `MAX_SESSION_READ_RESPONSE_BYTES`; byte truncation keeps the newest UTF-8 text and sets `truncated`. ANSI is a reserved request value that v2 always rejects. |
| `session.output` | `SessionOutputParams` | `SessionOutputResult` | Reads a newest retained tail or continues from an exact runtime-scoped output cursor. A request with `wait_ms` uses a dedicated connection with bounded-wait headroom. |
| `session.wait` | `SessionWaitParams` | `SessionWaitResult` | Performs one bounded long poll for state, activity, metadata, terminal, output, or runtime change. It always uses a dedicated connection. |
| `session.report_agent` | `SessionReportAgentParams` | `SessionReportAgentResult` | Hook callback for nested agents running inside an existing session. It records an active-agent claim, optional process binding, and optional active native metadata without changing launch identity or resume binding; ignored reports return `recorded: false`. Claims are reconciled with process facts and can be auto-released when no live backing process remains. |
| `session.release_agent` | `SessionReleaseAgentParams` | `SessionReleaseAgentResult` | Hook callback that clears a matching active nested-agent report and restores the session's default detector identity. Claude `SessionEnd` hooks use this as the clean-exit fast path; non-current releases return `released: false`; process-backed auto-release uses the same clear path. |
| `session.report_native_id` | `SessionReportNativeIdParams` | `SessionReportNativeIdResult` | Hardened public fallback for launch-agent resume metadata. Reports are runtime- and process-bound, ordered, expiring, and provider-matched; ignored reports return `recorded: false`. The owner-private worker claim path is preferred. This is not the nested-agent active identity callback. |
| `session.set_metadata` | `SessionSetMetadataParams` | `SessionSetMetadataResult` | Merges owner-controlled metadata. Values must not contain secrets. |
| `session.rename` | `SessionRenameParams` | `SessionRenameResult` | Sets or clears a session's owner display name (`name: null` clears). Cosmetic; the daemon trims it and rejects a control character or over-long name. |
| `session.diff` | `SessionDiffParams` | `SessionDiffResult` | Computes a unified diff of a session's worktree against a base ref. `base: null` defers to the worktree binding's recorded base branch, then the repository default. A session without a bound worktree returns `session_no_worktree`; a hostile explicit `base` (empty, leading `-`, or a control character) returns `invalid_branch`; a `base` that cannot be resolved to a merge-base against `HEAD` returns `session_diff_base_unresolved`. See `SessionDiffResult` under Core Payloads for the size cap and truncation semantics. |
| `session.policy.get` | `null` | `SessionPolicyResult` | Reads the daemon's session retention policy. Non-null params are `daemon/bad_request`. |
| `session.policy.set` | `SessionPolicyParams` | `SessionPolicyResult` | Validates, replaces, and persists the session retention policy at `<data_dir>/session-policy.json`. The running sweep task reloads it on its next cycle, so no daemon restart is needed. |
| `session.retention.sweep` | `SessionRetentionParams` or `null` | `SessionRetentionResult` | Runs one retention sweep under the current policy TTLs, or reports the selection when `dry_run` is true. It works whether or not automatic sweeps are enabled, never exceeds the policy's own per-sweep removal cap, and never selects an `external` session or one in `conflict`/`incompatible` runtime state. A matched session whose pohunek-owned worktree has an uncommitted change, an untracked file, commits contained in no other branch/remote/tag, or a state git cannot report is **held**, never removed: it is counted in `held`, excluded from `eligible`, and carries the `hold` reason (`worktree_uncommitted`, `worktree_untracked`, `worktree_unpushed`, `worktree_unknown`) on its candidate. One sweep inspects at most 64 checkouts; a matched session beyond that bound is held as `worktree_unknown` and inspected by the next sweep. `worktrees_cleaned` counts only checkouts confirmed gone from disk; a checkout that survived its removal is reported in `worktrees_failed`. The `pohunek session retention sweep` CLI exits non-zero when `failed` or `worktrees_failed` is above zero. |
| `subscribe` | `null` | `{subscribed: true}` then event stream | Consumes the connection into a one-way event stream. |
| `integration.install` | `IntegrationInstallParams` or `null` | `IntegrationInstallResult` | Installs agent hooks for active-agent state, native session id capture, provider-managed subagent lifecycle, and notifications. Each report carries `cleanup_incomplete`, the quarantine paths of replaced originals whose deletion did not finish after the install committed (empty when cleanup completed). |
| `integration.status` | `IntegrationStatusParams` or `null` | `IntegrationStatusResult` | Returns a read-only per-agent report for managed Codex and Claude hooks: availability, expected and present asset paths, inspected registration paths, installed and expected versions, aggregate health (`not_installed`, `current`, or `outdated`), typed recovery (`none`, `reinstall`, or `repair_configuration`), and non-secret warnings. `null` selects both agents; unknown parameter fields return `bad_request`. `current` requires exact managed executable permissions and exactly one registration under each installer-owned event. Inspection is size-bounded and runs outside the Tokio request task. It never mutates provider configuration. |
| `integration.uninstall` | `IntegrationUninstallParams` (`agent` required) | `IntegrationUninstallResult` | Removes exactly the assets and registrations the installer owns for Codex and Claude, as one rollback-protected transaction under the installer lock: registration first, then Codex trust records, then marker-carrying scripts. User hooks, the Codex hooks feature flag, the hooks directory, and the lock file are left alone; a symlink, directory, FIFO, or unmarked file at a managed script path is preserved and reported in `preserved_paths`. Each report carries `state` (`removed` or `not_installed`), `removed_paths`, `updated_paths`, `preserved_paths`, and `cleanup_incomplete` (quarantined originals whose deletion did not finish after the removal committed). `agent` names the one agent to remove and is required: a missing agent, unknown fields, and `null` params return `bad_request`, so removal never widens to every agent and a failure cannot leave one agent removed without a report. Ownership is judged on the inode that is deleted, and a provider file whose content, mode, or inode changed since it was read is a collision. The same re-verification runs before any rollback, so a file another writer changed is kept there too rather than replaced by the original. Every rollback or cleanup outcome that leaves data in quarantine is reported with its true path, as `integration_recovery_required` during a rollback and as `cleanup_incomplete` after commit. A registration file the removal wrote that another writer changed before the scripts are removed, or before the removal completes, is also a collision: it is re-verified (inode, mode, complete content), the other version is kept, and the removed scripts are moved back. Collisions, interrupted rollbacks, and concurrent installers return `integration_destination_collision`, `integration_recovery_required`, and `integration_install_in_progress`. |
| `integration.doctor` | `IntegrationDoctorParams` or `null` | `IntegrationDoctorResult` | Read-only diagnosis of the managed Codex and Claude hooks: per agent `ok`, the status report (absent while an operation is running), and findings with a stable snake_case `code`, `severity` (`info` or `error`), `summary`, and `remediation`. Only the absence expected of a not-installed integration is informational: an unsafe, symlinked, or group-writable config path is an error even when nothing is installed. Codes cover absent agents and hooks, config-root, asset, registration, provider-config, and Codex feature/trust drift, `displaced_original_left_behind` (a quarantined original from an earlier install or removal remains in the agent config directory) and `quarantine_scan_incomplete` (the bounded scan could not cover every entry, so it is an error and the diagnosis is never clean), and `operation_in_progress` (info: another install, uninstall, or doctor holds the installer lock, so nothing is read or scanned for that agent, `status` is absent, and the doctor stays `ok`; the doctor takes the lock non-blocking without creating the file and holds it for its whole inspection, so an installer arriving meanwhile gets `integration_install_in_progress`), `unsafe_installer_lock` (error: the lock file is not a regular owner-private file, so every install and uninstall fails), `install_drift`, and three informational `python3` notes that never fail the doctor or change its exit code, because the daemon cannot know the agent's `PATH`: `hook_runtime_python_found` (the first executable `python3` behind an absolute entry of the daemon's own `PATH`), `hook_runtime_missing` (none found; relative and empty entries are skipped), and `hook_runtime_macos_shim` (the first one is the `/usr/bin/python3` stub, recognized through any alias, with no Command Line Tools or Xcode behind it); the probe never runs an interpreter and the remediation is to verify `python3` on the agent's `PATH`, and `hook_socket_path_invalid` (daemon or worker socket path over the platform limit). The runtime notes apply only to an agent whose hooks are installed. `null` selects both agents; unknown fields return `bad_request`. |
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
| `package.list` | `null` | `PackageListResult` | Local-only. Lists installed runtime packages in ascending digest order with the registry `generation` and per-package health. See [Runtime Package Methods](#runtime-package-methods). |
| `package.inspect` | `PackageInspectParams` | `PackageInspectResult` | Local-only. Returns one installed package record and what its runtime descriptor declares. |
| `package.doctor` | `PackageDoctorParams` or `null` | `PackageDoctorResult` | Local-only. Verifies every installed package root and reports faults, unregistered roots and pinned digests that are not installed. |
| `package.install` | `PackageInstallParams` | `PackageInstallResult` | Local-only. Installs a runtime package archive under explicit-digest or catalog trust, or previews it with `dry_run`. |
| `package.link` | `PackageLinkParams` | `PackageInstallResult` | Local-only. Copies a developer package directory into content-addressed storage and installs it disabled and unselected. |
| `package.set_enabled` | `PackageSetEnabledParams` | `PackageChangeResult` | Local-only. Enables or disables an installed package for fresh launches. |
| `package.select` | `PackageSelectParams` | `PackageChangeResult` | Local-only. Selects the installed version bare requests of its package id resolve to. |
| `package.uninstall` | `PackageUninstallParams` | `PackageUninstallResult` | Local-only. Removes an installed package that no session or host profile pins. |

`status` exists as a method constant in `crates/protocol` but is not a supported
daemon method in this API version. It returns `daemon/method_not_found`.

`HostCapabilities` advertises `terminal_read_supported`,
`output_read_supported`, and `session_wait_supported` independently. Clients
must check the relevant flag instead of assuming that a reachable daemon or an
attach-capable session supports every observation method. Its `runtimes` entries
are live host-local probes: `agent` is the selected profile or base name and
optional `agent_base` is the `RuntimeRef` of the runtime behind it. Optional
`version` and `supported` are a provider policy, not generic availability:
their absence means that no version policy applies. The daemon builds the
inventory from its runtime registry: a runtime whose definition names a
version-probe parser reports a policy, and a present runtime with a policy
always reports `supported` as `true` or `false`. Clients derive launchability
from the entry alone: `available` and `supported != false`; an entry with no
`supported` has no policy, and no runtime name or `agent_base` is special-cased.
An unavailable runtime with a policy omits both fields. For Hermes, `available:
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
profile inventory, but a present historical label (a value outside the runtime-id
grammar) is presentation-only and not launchable.

### Runtime Package Methods

The `package.*` methods manage the runtime packages installed on the host the
daemon runs on. They are local-only: the daemon serves them on its local Unix
control socket and refuses them on a remote overlay TCP connection with
`local_only_method`, because installing a package extends the owner's launch
authority on that machine. They are additive: the protocol version does not
change. A package is addressed by `digest` (`sha256:` plus 64 hex characters),
the content address of its verified archive. Payloads never carry file
contents, package paths or secret values.

Shared payloads:

- `PackageInfo`: `digest`, `package` (`{id, version}` from the runtime
  descriptor), `origin` (`official`, `explicit_digest` or `link`), `enabled`,
  `selected`, `installed_at_unix_seconds`, optional `runtime_id`, optional
  `fault`, and `referenced` (a live, lost or resumable session or a host
  profile pins the digest).
- `PackageFault`: `root_missing`, `root_modified`, `root_unsafe`,
  `root_unreadable`, `descriptor_missing`, `descriptor_invalid`,
  `identity_mismatch`, `runtime_not_claimable`, `runtime_conflict`. `fault` is
  absent when the root verified and the descriptor loaded.
- `PackageRuntimeInfo`: `runtime_id`, `display_name`, `program`, `args`,
  `resumable`, `forkable` and optional `integration_handler`. The program and
  fixed arguments are exactly what the daemon launches as the owner, so they
  can be reviewed before an install.
- `PackageTrust`, a tagged object: `{"kind":"explicit_digest","digest":...}`
  requires the archive digest to equal `digest` and never authorizes an
  official runtime alias; `{"kind":"catalog","catalog_path":...}` names a
  signed catalog at an absolute path on the daemon host.

| Method | Params | Result | Behavior |
|---|---|---|---|
| `package.list` | `null` | `{generation, packages[]}` | `generation` counts committed registry changes. |
| `package.inspect` | `{digest}` | `{package, runtime?}` | `runtime` is absent when the descriptor cannot be loaded. |
| `package.doctor` | `{package?}` or `null` | `{generation, findings[]}` | `package` restricts the report to one package id. A finding is `{kind, digest, package?, fault?, referenced}`; `kind` is `fault`, `unregistered_root` (a root on disk without a record; installing the same archive again adopts it after verification) or `pinned_not_installed` (a session or host profile pins a digest the registry does not record). An empty list means everything verified. |
| `package.install` | `{archive_path, trust, enable, select, dry_run}` | `{status, package, runtime, reloaded}` | All fields are required. `archive_path` is absolute on the daemon host. `status` is `preview` (dry run, nothing changed), `installed`, `already_installed` (recorded with a verified root) or `root_restored` (recorded, root was missing and extracted again). A package that is already recorded keeps its recorded `enabled`, `selected` and origin: `enable`, `select` and `trust` apply to a new record only. |
| `package.link` | `{directory, dry_run}` | `{status, package, runtime, reloaded}` | Copies the directory into content-addressed storage and installs it disabled and unselected; the daemon never loads the directory itself. |
| `package.set_enabled` | `{digest, enabled}` | `{package, reloaded}` | Disabling blocks fresh launches only; a session already pinned to the digest still resumes. Enabling first proves the package root verifies and the package may serve its runtime id, so it cannot create a runtime id conflict. |
| `package.select` | `{digest}` | `{package, reloaded}` | Bare requests of the package id resolve to this version afterwards. The same root and runtime id checks as enabling apply. |
| `package.uninstall` | `{digest, remove_modified}` | `{digest, reloaded}` | Refused with `package_referenced` while a session or host profile pins the digest. `remove_modified: true` removes only a root that fails verification; a verified root is refused with `package_root_intact`. |

Install and link validate the package in memory before anything is extracted:
`runtime.toml` is parsed from the verified archive, and the package identity
(id and version) comes from that descriptor. `shell` is never claimable. The
official aliases (the reserved runtime ids other than `shell`) are served only
by a package the signed catalog authorizes as official, and only while no
built-in runtime serves them. A runtime id served by a loaded package of a
different package id is a conflict. The runtime registry reserves the official
aliases for built-in runtimes, so an alias claim is answered with
`package_runtime_conflict` even for an official package, and a claim by an
explicit-digest or linked package with `package_runtime_not_claimable`; a
descriptor that names `shell` is `package_runtime_not_claimable` as well.
Catalog trust fails closed with
`official_trust_unavailable` on a host without a trust anchor, which is every
production host today.

A catalog install checks the catalog signature, the persisted high-water
sequence and revoked key ids, then that an entry binds the package id, runtime
id, version and digest and supports this core version and platform; after the
install it persists the catalog's sequence and revocations, and a failure to do
so is answered with `package_registry_failed` once the package is installed.

After each committed change the daemon rebuilds its runtime registry and
reports `reloaded`. Every mutation holds the lifecycle guard, so launches never
interleave with it. A modified root makes enable, select and uninstall fail with
`package_root_invalid`; reinstall the package or uninstall it with
`remove_modified`.

Errors use the error contract below. Every code has fixed message and
`recover` text that carries no path, package content or caller-supplied value.

| Code | Class | Meaning |
|---|---|---|
| `local_only_method` | `daemon` | The request arrived on a remote overlay connection. |
| `package_archive_invalid` | `runtime` | The archive is not a valid canonical package archive, breaks a limit or has another digest than the trust names. |
| `package_source_unreadable` | `runtime` | The archive file, package directory or catalog could not be read or is not an absolute path. |
| `package_untrusted` | `runtime` | The trust does not authorize the archive. |
| `official_trust_unavailable` | `configuration` | The host has no catalog trust anchor, so an official package cannot be authorized. |
| `package_incompatible` | `runtime` | The package does not support this core version or platform. |
| `package_descriptor_invalid` | `runtime` | The package's runtime descriptor is not a valid runtime definition. |
| `package_runtime_not_claimable` | `runtime` | The package claims a runtime id it may not serve. |
| `package_runtime_conflict` | `runtime` | Another package or a built-in runtime serves the runtime id. |
| `package_not_installed` | `runtime` | The digest is not installed. |
| `package_identity_installed` | `runtime` | The package id and version are installed from another archive. |
| `package_identity_conflict` | `runtime` | The digest is installed under another package identity. |
| `package_referenced` | `runtime` | A session or host profile still pins the digest. |
| `package_root_invalid` | `runtime` | The recorded package root fails verification. |
| `package_root_intact` | `runtime` | The recorded package root verifies, so it is not removed as modified. |
| `package_registry_busy` | `daemon` | Another writer holds the package registry. |
| `package_registry_failed` | `runtime` | The package registry or its storage failed. |
| `package_limit_reached` | `runtime` | The registry holds the maximum number of packages. |
| `package_reload_failed` | `daemon` | The change was committed but the runtime registry was not rebuilt. |

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

### `HostRecord`

- `overlay`: required stable transport ID used to qualify peer identity.
- `peer_id`: optional stable provider peer identity; absence is preserved.
  NetBird uses `publicKey` or legacy `pubKey`, never the mutable peer IP.
- `address`: optional dialable IP address without a port. `null` keeps an
  address-less or rejected-spoof candidate visible but non-dialable.
- `port`: required effective daemon port for this overlay route.
- `name` and `fqdn`: optional display selectors, never sufficient by themselves
  to bypass collision checks.
- The flattened host class remains `candidate`, `reachable_daemon`,
  `unreachable`, or `version_mismatch`.

### `HostGovernanceStatus`

`host.governance.inspect` has no parameters: explicit `params: null` and
omitted `params` both succeed under the general parameterless-method rule.
Every non-null JSON value, including an object, array, string, number, or
boolean, returns the canonical `daemon/bad_request` error. Its result is a
strict, public-safe object with exactly these six required fields. All six keys
are always present. `host_id` and `approval_key_reference` are always non-null
canonical IDs; only `enrollment`, `owner`, `owner_revision`, and `quarantine`
are conditionally nullable according to the lifecycle invariants below. Unknown
or missing fields are not a valid result:

- `host_id`: the daemon's stable opaque `HostId`. It is distinct from a CLI
  or overlay route selector and from every other typed identity.
- `enrollment`: `null` for a never-enrolled host, otherwise the one local
  enrollment's relay ID, status, and canonical non-zero enrollment revision.
- `owner`: `null` for a never-enrolled host, otherwise exactly one tagged
  principal or team owner. Co-ownership is not representable.
- `owner_revision`: `null` exactly when both `enrollment` and `owner` are
  `null`; otherwise a canonical non-zero decimal string.
- `quarantine`: `null` unless the enrollment status is `quarantined`; then it
  contains the explicit quarantine reason.
- `approval_key_reference`: the required public reference to the host approval
  verification key. It is safe to inspect but is not signing material.

The public result never contains an approval private key or seed, a proposal,
transfer outcome, signature, nonce, retired-enrollment summary, relay
credentials, or other private persistence data. The daemon validates all of
those local records before publishing the snapshot. IDs are opaque canonical
values and must not be parsed for routing or authorization meaning. Revisions
are checked monotonically by the local governance state; JavaScript clients
receive their decimal strings rather than lossy numbers.

If the daemon cannot safely obtain the retained governance snapshot, the method
returns exactly `daemon/host_governance_unavailable` with the fixed message
`host governance status is unavailable` and recovery hint `reload or restart
the daemon, then retry`. It does not reveal a state path, raw I/O error, record
contents, key material, proposal, nonce, signature, or previous snapshot.

### Native reference strategies

A runtime definition declares in `[native_reference]` how core obtains the native
session reference that resume and fork consume. The table is required.

| `strategy` | Reference source | Resume and fork |
|------------|------------------|-----------------|
| `none` | none | frozen off; requires `resume.supported = false` |
| `hook` | a validated integration report (process-bound, ancestry, sequence and expiry checked) | on once a report arrived; requires a supported resume |
| `assigned` | generated by core and passed to the agent at launch | on from the first moment; requires a supported resume |

The built-in runtimes declare `shell` = `none` and `codex`, `claude`, `hermes`
= `hook`, so their behavior is unchanged.

Rule for runtime packages without an integration handler: such a package gets
terminal launch and activity detection. It gets resume and fork only when its
agent CLI accepts a caller-chosen session id and the definition declares
`strategy = "assigned"`; with `hook` or `none` it is not resumable. A package
cannot obtain a reference through `session.report_native_id`, because that
method accepts only a report from the launch process itself.

`assigned` takes, in addition to a supported `[resume]` with
`reference_kind = "id"` (core can generate an id, not a path):

```toml
[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{reference}"]   # passed at launch

[native_reference.existence]                     # required, explicit
check = "file"                                   # or "none"
root_env = "PI_CODING_AGENT_DIR"                 # config home variable
root_home = ".pi/agent"                          # used when the variable is unset
dir = "sessions"                                 # directory below the root
file_name = "_{reference}.jsonl"                 # exactly one {reference}
name_match = "ends_with"                         # or "exact"
max_depth = 1                                    # directory levels below dir
```

- `launch_args` follows the same whole-token `{reference}` rules and bounds as
  `[resume] args`. At launch the daemon generates a fresh hyphenated UUID (never
  taken from a client or user), appends the rendered `launch_args` after the
  runtime's fixed arguments and before the initial prompt argument, and stores
  the reference in the session and its recovery binding before the agent starts.
  Resume and fork use the declared `[resume]` / `[fork]` argv with that
  reference; the start flag is never repeated.
- The binding records a provenance: `assigned` for a generated reference,
  `reported` for one a validated report delivered (also the value for records
  written before the field existed). An assigned reference is not identity
  evidence: it creates no ordering key and takes no part in ancestry, sequence,
  expiry or pid-identity validation, which stay mandatory for reports. Any
  write of the stored reference by a validated report labels it `reported`, so
  the provenance is never stale.
- An assigned reference goes stale when the user changes conversation inside the
  agent (for example `/clear` or an in-session resume), and it names nothing
  when the agent never wrote the conversation. Before `session.resume` or
  `session.fork` launches anything, the declared `existence` check runs against
  the frozen declaration. `check = "none"` skips verification and relaunches
  from the reference unchecked. `check = "file"` lists directories below the
  config home (the environment variable named by `root_env`, else `root_home` below `HOME`;
  both are read from the environment the agent is launched with: the daemon's
  allowlist-filtered base environment, overridden by the session's profile
  environment, so a variable or `HOME` the agent would not see is not used), descends at most `max_depth` levels below `dir`
  (at most 4) visiting at most 50 000 entries, and matches regular files whose
  name equals (`exact`) or ends with (`ends_with`) `file_name` with the
  reference substituted. It runs no shell and expands no glob. It opens the
  root once and reads every directory through descriptors opened relative to
  their parent without following symlinks, so a symlink, or a directory swapped
  for one during the scan, is never entered and fails closed. `root_env`, `root_home`,
  `dir` and `file_name` accept only plain components (ASCII letters, digits,
  `.`, `_`, `-`), so no path, `..`, glob or `POHUNEK_*` variable is
  expressible. A missing conversation, an unset or relative root, an
  unverifiable reference or a store over the entry bound fails closed with
  `runtime/agent_native_reference_missing`; recovery never falls back to another
  runtime, to the shell or to "continue latest". The `ends_with` form matches any
  file whose name ends with the rendered text, so the template must start with
  the separator the agent writes (`_` above).
- A fork of an assigned reference starts a new conversation whose reference core
  cannot learn without an integration report, so the forked session holds no
  reference and is not resumable until a validated report arrives.
- A host profile on an `assigned` base inherits the assignment; one that
  restates `[resume]` is rejected with `invalid_profile`, and
  `resumable = false` switches recovery off (no reference is generated).
- Not yet shipped: the handling of `/clear`-style conversation switches, where
  a later `reported` reference supersedes an assigned one, with its fixture
  tests, lands with the integration-report work (#144, #52). A package without
  an integration cannot send a report.
- A forked session of an assigned reference still shows the frozen
  `capabilities.resume = true` but holds no reference, so `session.resume`
  answers `not_resumable` for it.
- The Pi values in the example are illustrative; verify a runtime's flag and
  on-disk layout before declaring them.

### `SessionInfo`

Important fields:

- `id`: stable session id.
- `external`: optional bool. `false` means a normal pohunek-owned PTY session;
  `true` means an opt-in observed external agent. External sessions are
  read-only: attach, input, resize, stop, remove, rename, metadata updates, and
  resume return `runtime/session_external_read_only`.
- `capabilities`: required `SessionCapabilities` object with `resume` and
  `fork` booleans frozen for the logical session. Both derive from one frozen
  native-session launch spec (reference kind, resume argv, optional fork argv),
  so `fork` is `true` only when `resume` is `true`. Clients must use these flags
  instead of inferring provider behavior from `agent` or `agent_base`. Records
  that predate the field load both flags as `false`. Host profiles declare the
  spec in `[resume]` (`reference_kind`, `args`, optional `fork_args`, accepted only on
  bases with compiled fork support, currently `claude`); see the
  agent-profiles concept page.
- `name`: optional owner-set display name; absent means the session is shown by
  its id. Set at `session.new` and changed via `session.rename`.
- `agent`: profile name.
- `agent_base`: the `RuntimeRef` of the runtime backing the session, for
  example `shell`, `codex`, `claude`, or `hermes`; a historical label outside
  the runtime-id grammar remains presentation-only.
- `active_agent`: optional runtime agent profile currently active inside the
  session. Present for nested agents reported through hooks or inferred from
  process facts.
- `active_agent_base`: optional `RuntimeRef` of the runtime backing
  `active_agent`.
- `active_agent_pid`: optional process id backing `active_agent`. When present,
  the daemon validates it with kernel process-start identity and auto-releases
  the active agent if that exact process exits. Foreground reconciliation uses
  the terminal PGID as the primary selection hint; structured daemon diagnostics
  report the selected `focus_pid` and `foreground_pgid` when either changes.
- `active_agent_session_id` / `active_agent_session_path`: optional native
  metadata for the active nested agent. These fields are display/runtime
  metadata only and do not make the parent session resumable as that nested
  agent.
- `subagents`: provider-managed child agents observed through current Claude
  and Codex lifecycle hooks. Each item contains `id`, optional `parent_id`,
  `provider`, optional `agent_type`, `lifecycle`, optional coarse `activity`, a
  decimal-string `revision`, worker timestamps, and optional terminal time.
  Multiple children may run concurrently. The bounded recent history survives
  daemon/client reconnects; running children become `lost` when the owning PTY
  runtime ends. No prompt, result, transcript path, or raw hook payload is
  exposed.
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
  `incompatible`; `worker_id` identifies the PTY owner and `worker_instance_id`
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
  `link.kind`, `link.id`, `link.url`, `link.branch`) written by external clients
  (such as the native GUI) and the launch scripts to tie a session to a work item — no protocol surface
  is dedicated to it.
- `created_at`, `updated_at`: RFC3339 timestamps.
- `exit_code`: optional process exit code.

`RuntimeRef` wire values (`agent_base`, `active_agent_base`, the subagent
`provider`, the inventory `agent_base`, the integration `agent` and the
notification `agent_kind`) are forward-compatible for presentation. An unknown
string round-trips through Rust and TypeScript clients as a neutral value.
Agent-targeted mutation, resume and fork resolve the value through the daemon's
runtime registry: a value outside the runtime-id grammar is rejected with
`runtime/agent_kind_unsupported`, and a grammar-valid id that no enabled runtime
resolves is rejected with `runtime/runtime_not_installed`. Unknown values never
silently become a supported launch, resume, or fork runtime. A session or
recovery record whose runtime is not installed is kept inert: it stays listed
with its stored recovery binding, refuses resume, fork and mutation with
`runtime_not_installed`, and resumes again once the runtime is installed. The
binding records the runtime identity (`LaunchBinding`) the session was
launched with; a binding written without one may only resume through a
built-in runtime. A session launched from an installed runtime package pins the
package archive digest and resumes only from exactly that digest, verified
again; installed content that is missing, modified or contradicts the pin is
rejected with `runtime/runtime_incompatible` and never replaced by another
package or a built-in.

A launch resolves its agent through a launch source. The owner-local source
accepts a host profile name or an installed runtime id. A relay-selected launch
(connected by the `HostShare` work of #82; no relay caller exists yet) resolves
only a locally approved host profile by name and approved revision. It cannot
name a runtime id, package, program or argv: a bare runtime id is rejected with
`runtime/agent_profile_not_found`, and a profile whose file, detection
manifest, base-runtime launch binding or effective program and arguments
changed after approval is rejected with `runtime/agent_profile_revision_stale`.
A revision is a keyed MAC (host-local secret), 64 lowercase hex digits, so it
reveals nothing about profile `[env]` values. A malformed revision is
`runtime/agent_profile_revision_invalid`; an unreadable host key is
`runtime/agent_profile_revision_unavailable` (no unkeyed fallback).

Agent runtime identities use two Rust types over the same string namespace.
The fields listed above (`agent_base`, `active_agent_base`, the subagent
`provider`, the inventory `agent_base`, the integration `agent` and the
notification `agent_kind`) are `RuntimeRef` values, so their TypeScript type is
the `RuntimeRef` string alias and their wire form is a bare string; the daemon
validates them against the registry before a mutation.
`RuntimeId` is always valid: lowercase ASCII alphanumerics plus `.`, `_` and
`-`, 1 to 64 bytes, no leading `.` or `-`, no `..`; deserialization rejects
anything else. `RuntimeRef` is lenient: any string deserializes, a
grammar-valid value is an `Id` and every other value is a historical label,
displayable but rejected with `runtime/agent_kind_unsupported` (a fixed message
that never echoes the label). The wire form is a bare string, so a reference
round-trips to the same kind. `Id` does not mean launchable: installation is
decided by the daemon registry at resolve time, and a valid `RuntimeId` that no
enabled runtime resolves is rejected with `runtime/runtime_not_installed`.
`PackageId`, `PackageVersion`, `PackageDigest` and `DescriptorDigest`
(`sha256:` plus 64 lowercase hex characters) identify where a runtime
definition came from. `PackageIdentity` pairs a package id with its version.
`LaunchBinding` pairs a `RuntimeId` with a `provenance`: `builtin` carries an
optional `package` identity (id and version together or not at all) and a
descriptor digest that covers only structural launch fields and is not a
package digest, `package` carries a `package` identity and a package digest.
All are TypeScript strings except `PackageIdentity`, `LaunchBinding` and
`BindingProvenance`.

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
  "worker_instance_id": "runtime-1",
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

`session.detection` accepts the same single-session parameter shape:

```json
{"session_id":"s-42"}
```

It asks the live detector task to render its active manifest regions on demand;
previews are not copied for every PTY output chunk. The result lists every
region kind supported by this engine, then only the regions required by the
active manifest in manifest order:

```json
{
  "session_id": "s-42",
  "supported_regions": [
    "osc_title",
    "osc_progress",
    "whole_recent",
    "bottom_lines",
    "bottom_non_empty_lines",
    "top_non_empty_lines",
    "last_non_empty_above_prompt_box",
    "after_last_prompt_marker",
    "prompt_box_body",
    "after_last_horizontal_rule"
  ],
  "previews": [
    {
      "kind": "top_non_empty_lines",
      "region": "top_non_empty_lines(8)",
      "text": "Do you trust the contents of this directory?"
    }
  ]
}
```

Parameterized manifest syntax is `bottom_lines(N)`,
`bottom_non_empty_lines(N)`, or `top_non_empty_lines(N)`; the preview's
`region` preserves the canonical count while `kind` stays count-independent.
`top_non_empty_lines(N)` returns the first `N` non-empty visible rows.
`last_non_empty_above_prompt_box` returns the nearest non-empty row above the
second horizontal rule counted from the bottom, or an empty preview when a
complete prompt box or preceding content is absent. All screen regions use the
same visible-grid, wide-glyph, and soft-wrap semantics as activity matching.
Unknown region names remain a typed manifest parse failure, so older engines
reject manifests that use regions they do not implement instead of silently
over-matching another surface. A preview observes every accepted detector
configuration update before it renders, so it cannot return regions from the
previous active manifest after an agent report or release succeeds. A stopped
or unavailable detector returns `session_terminal_unavailable`. If the complete
success envelope would exceed the 1 MiB control-line cap, the daemon returns the
payload-free `runtime/session_detection_response_too_large` error instead.

`session.read` accepts an optional source (`visible`, `recent`,
`recent_unwrapped`, or `detection`; default `visible`) and optional `lines`
(`1..=1000`, default `1000`). Current workers expose only the current rendered
screen and do not expose scrollback, soft-wrap metadata, or one canonical
detection text. Requests for `recent`, `recent_unwrapped`, or `detection`
therefore safely fall back to visible rows and report `source_used: "visible"`.
`alternate_screen` always reports the captured terminal's real buffer state, so
callers can recognize alternate-screen fallback without a false history claim.
Line and byte truncation both retain the newest tail.
The result carries exact runtime identity after post-snapshot verification,
canonical decimal-string `revision`, effective `lines_requested`, and
`truncated`. The daemon caps the complete JSON-serialized result at
`MAX_SESSION_READ_RESPONSE_BYTES` (1,048,418 bytes). `format: "ansi"` is
reserved for future raw capture support and currently returns
`runtime/session_read_ansi_unavailable`.

`session.output` uses an optional nested runtime identity and an exclusive
cursor. Omitting `after_offset` requests the newest retained tail. A cursor
requires its exact runtime identity, and `wait_ms` requires a cursor:

```json
{"session_id":"s-42","max_bytes":65536}
```

The initial-tail request deliberately has no runtime or offset. Persist the
returned `worker_instance_id`, `runtime_generation`, and `next_offset` before issuing a
cursor-based read:

```json
{
  "session_id": "s-42",
  "runtime": {"worker_instance_id": "runtime-1", "runtime_generation": "3"},
  "after_offset": "2",
  "max_bytes": 65536,
  "wait_ms": 5000
}
```

The result returns standard base64 and every cursor needed to continue:

```json
{
  "session_id": "s-42",
  "worker_instance_id": "runtime-1",
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
  "runtime": {"worker_instance_id": "runtime-1", "runtime_generation": "3"},
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
`session_read_ansi_unavailable`, `session_output_limit_exceeded`,
`session_wait_limit_exceeded`,
`session_waiter_limit_reached`, and `worker_feature_unavailable`. Restart from a
fresh screen/tail after runtime change. A worker on the immediately preceding
private protocol remains usable for existing lifecycle and attach operations,
but observation returns `worker_feature_unavailable`.

### Active-Agent Hook Payloads

Managed PTY children inherit these reserved environment values. The worker
injects them itself, after any base, profile, or daemon-supplied variable, so
nothing else can shadow them:

- `POHUNEK_ENV=1`
- `POHUNEK_SESSION_ID`
- `POHUNEK_WORKER_ID`
- `POHUNEK_WORKER_INSTANCE_ID` (identifies one worker instance, the PTY
  generation that the public session runtime reports as `worker_instance_id`)
- `POHUNEK_WORKER_SOCKET_PATH`
- `POHUNEK_WORKER_HOOK_PROTOCOL_VERSION` (private worker-hook protocol version)
- `POHUNEK_SOCKET_PATH` for daemon-targeted notification delivery
- `POHUNEK_PROTOCOL_VERSION` (public daemon RPC protocol version required by
  the provider hooks)
- `POHUNEK_NATIVE_REFERENCE_KIND` when, and only when, the launch carries a
  native reference kind

A managed child does not inherit the daemon's or the worker's process
environment. Its environment is built from an empty base: the allowlisted
variables the daemon passes from its own environment (the `[environment]
allowlist` of `service.toml`, by default `PATH`, `HOME`, `USER`, `LOGNAME`,
`SHELL`, `LANG`, `LC_*`, `TMPDIR`, `SSH_AUTH_SOCK`, `DISPLAY`,
`WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, and `XDG_*`), `TERM`, the agent
profile's environment, and the reserved values above. Ambient `POHUNEK_*`
markers coming from the daemon or the profile are stripped first, so only the
worker's authoritative reserved values reach the child. Service-manager
variables (`NOTIFY_SOCKET`, `WATCHDOG_*`, `INVOCATION_ID`, `JOURNAL_STREAM`,
`MANAGERPID`, `SYSTEMD_EXEC_PID`, `XPC_SERVICE_NAME`, `XPC_FLAGS`,
`__CFBundleIdentifier`, `LaunchInstanceID`) and the worker-authentication
tokens `POHUNEK_CONTROLLER_TOKEN` and `POHUNEK_BOOTSTRAP_TOKEN` are always
removed, even when a profile or an allowlist would supply them; the token
values never reach agent code. The `[environment]` table of `service.toml` also
requires a `search_path` list: the absolute, normalized, non-repeating
directories the installer resolved and hands to the daemon job as its `PATH`
(an empty list keeps the service manager's own `PATH`; the joined value is
bounded). Each directory is recorded as its canonical path and was trusted when
resolved (owned by the user or root, not writable by group or others). The path is
recorded at install and reaches the daemon job only when the installer writes
the job definition (install, or an upgrade to a different version); upgrades
reuse the recorded list, so a manual edit takes effect only at the next
version-changing upgrade, and an in-place refresh is tracked in #319. This key
raised the `service.toml` `schema_version` to 2; a version 1 file is refused
with a message to uninstall with the pohunek that wrote it and install again,
because there is no migration. `pohunek service install --json` reports the
outcome in an additive `search_path` object: `source` (`login_shell`,
`fallback`, `recorded`, `unmanaged`), `entries`, `shell_used`,
`shell_defaulted`, `login_shell_failure` (the rendered typed reason when the
fallback list was used), and `dropped` (`[{path, reason}]`, directories refused
as untrusted); the human output prints a `warning:` line for a failed login
shell and for each dropped directory. A set but relative `$SHELL`, or a
non-UTF-8 `HOME`, `USER`, or `LOGNAME`, fails the install
(`service_environment_invalid`, `service_environment_not_utf8`). When the
installer cannot resolve the search path, `pohunek service` fails with the CLI
error code `service_search_path_unavailable`; an invalid list in an existing
file is `service_config_invalid`; resuming an interrupted install whose
`service.toml` names another prefix or version fails with
`service_resume_config_mismatch`. See
[environment resolution](knowledge/guides/environment-resolution.md). A worker kept
running across an upgrade from the previous private protocol version keeps
starting its session's children from its own sanitized environment until that
session gets a new worker generation.

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
strict params are `session_id`, `worker_instance_id`, `agent`, non-zero `pid`, decimal
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
  "worker_instance_id": "runtime-42",
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
`POHUNEK_INTEGRATION_VERSION=10`, run their interpreter in isolated mode (`-I`,
so the session working directory never shadows the standard library), read the
worker instance from `POHUNEK_WORKER_INSTANCE_ID` (falling back to
`POHUNEK_RUNTIME_ID`), read
provider JSON through a bounded direct pipe without staging it on disk, and send the hook process's parent PID on
`SessionStart`. An installed hook from an earlier asset version reads only
`POHUNEK_RUNTIME_ID`, which current workers do not set, so it cannot report identity; `integration.status` reports it as
`outdated` with `reinstall` recovery, and `integration.doctor` as an asset
finding, until it is reinstalled. A worker sets only
`POHUNEK_WORKER_INSTANCE_ID` and strips an inherited `POHUNEK_RUNTIME_ID` from its
children. The hooks and the process sweep also read `POHUNEK_RUNTIME_ID` as the
same worker instance marker, so descendants of a worker that set only that name
are still reaped and still report identity; a process whose two markers name
different instances, one of them the swept instance, is never signalled and
leaves the cleanup unconfirmed (a pair naming other instances only is ignored).

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
- `agent_kind`, `project_id`: optional display and filtering context;
  `agent_kind` is a `RuntimeRef`.
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

The state adapter also registers `SubagentStart` and `SubagentStop` for both
providers. These callbacks target only the owner-private worker endpoint and
silently no-op when it is unavailable. They copy only lifecycle identifiers and
agent type. The hook validates its action and Pohunek handshake before reading,
rejects oversized JSON, and never stages the payload on disk; provider prompts,
results, messages, and transcript paths are discarded.

The four integration methods classify an explicit `agent` before acting: a
value outside the runtime-id grammar is `runtime/agent_kind_unsupported`, a
valid id no enabled runtime backs is `runtime/runtime_not_installed`, and an
installed runtime with no daemon-managed hook integration (the shell, Hermes,
or any other registered runtime) is `runtime/agent_not_installable`.

`integration.status` is the corresponding read-only drift report. Bare status
reports both daemon-managed agents; `--agent codex` and `--agent claude` select
one. Every managed script is checked independently against its embedded asset
and executable mode. Claude must also contain every exact managed registration
in `settings.json`. Codex must contain every exact registration in `hooks.json`,
have the hooks feature enabled, and retain the position-derived trust hash for
each managed hook in `config.toml`. The managed Codex trust-key set must be
exact: a trust record is installer-owned when its trusted hash is that of a
managed command for its event, so a stale managed record at an old position
makes the integration outdated and reinstallation removes it before writing only
the currently expected keys, while trust records of a user's own hooks are never
removed or flagged and follow their hooks: a trust key embeds the group and
handler index, so when an install, reinstall, or uninstall shifts a user hook's
position its record moves to the key of the new position (the trusted hash covers
only the handler and its matcher, so it stays valid). Two claims on one key fail
closed with `configuration/integration_trust_conflict` and nothing is written. A scalar anywhere
in the managed trust namespace requires configuration repair, including when
hook drift temporarily prevents that key from being position-derived.

`not_installed` means no Pohunek-managed asset or registration was detected;
A missing agent config directory is `available: false` with `not_installed`, recovery `none`, and no warning; a path that exists but is not a directory, has a symlink at any component (live or dangling, which install and uninstall refuse), is refused by the same trusted descriptor walk the installer performs (foreign owner, group/world-writable directory or ancestor), cannot be resolved, or cannot be inspected is `outdated` with `repair_configuration`.
`current` means the complete contract above matches; `outdated` covers every
partial, modified, malformed, unreadable, or otherwise unverifiable detected
installation. A supported agent's path-resolution failure becomes an `outdated`
report with a non-secret warning; in aggregate mode it does not suppress the
other agent. Explicit unsupported agents still return a typed error. The response
reports real installed and expected version fields. `installed_version` is
unknown when any readable managed asset lacks a valid marker. The typed recovery
is `reinstall` only when the installer can repair every finding; malformed,
unreadable, oversized, or unresolved provider configuration instead reports
`repair_configuration`. Symlinked, special, foreign-owned, or group/world-writable
managed assets also require manual repair, while an explicit reinstall replaces
installer-owned assets atomically without following an existing symlink. Managed
asset type, effective-UID ownership, mode, and content are inspected through one
non-following descriptor. The relevant parent chain starts at the explicitly
resolved agent config root and ends at the asset's direct parent; every path in
that chain must be a real directory owned by the daemon's effective UID without
group or world write access. System ancestors above the config root are outside
this check, so ordinary safe home/XDG roots are not rejected merely because an
ancestor such as `/tmp` is shared. Each Claude `settings.json`, Codex
`hooks.json`, and Codex `config.toml` is opened without following symlinks and
must be a regular file owned by the daemon effective UID without group/world
write access; metadata and bounded content come from the same descriptor.
Provider inspection is nonblocking so FIFOs cannot stall the daemon worker. A
missing Claude `hooks/` child under an otherwise trusted config root is a
reinstallable absence and can still report `not_installed`; an existing symlink,
wrong type, foreign owner, or group/world-writable child requires
`repair_configuration`. The installer creates a missing Claude `hooks/`
directory with exact mode `0700` regardless of the inherited umask and removes
that newly created directory if mode enforcement or safe opening fails, but
never changes permissions on an existing real user directory. Codex trust
records describe a canonical single-handler managed
group, so adding a sibling handler makes status non-current until reinstall
separates the managed handler while preserving the user sibling. Agent config
roots selected through `CLAUDE_CONFIG_DIR` or `CODEX_HOME` must be absolute and
UTF-8 representable after tilde expansion. Invalid roots fail with
`agent_config_dir_invalid` before registration commands are constructed, so a
daemon working directory cannot change where a provider process resolves them.
Before any mutation, installation opens and validates every existing config and
hook parent as a no-follow, effective-UID-owned real directory without group or
world write access. An unsafe parent fails with
`configuration/integration_path_untrusted`. Provider files are prepared in
memory, then replaced through the already-opened directory descriptor by
displacement: the original is renamed aside into a private quarantine name
bound to its inode, its identity, mode, and complete content are verified on
that inode against what was read, and the new file, written to an exclusive
temporary file and synchronized, is activated with a no-replace rename. A
foreign change at any moment is therefore an
`integration_destination_collision`, and a foreign file created in the gap is
never overwritten; on any mismatch the original is moved back untouched. The
originals are deleted only after every file is in place; if that cleanup cannot
finish, the install still succeeds and `cleanup_incomplete` lists the quarantine
paths that remain, which `integration.doctor` reports as
`displaced_original_left_behind`. Parent or pathname swaps cannot redirect the
write. Existing safe provider-file modes are preserved,
while new registration files use mode `0600` and managed executable assets use
mode `0755`. An oversized file is `outdated` with an actionable warning. The CLI
routes Codex and Claude status to the effective global `--host`; mutating hook
installation and every Hermes lifecycle action remain local. Human recovery
hints and warning commands for a remote report explicitly name the daemon host
where the operator must run the local installer; passing that host back to
`integration install` is not a remote mutation. A JSON registration root that
is not an object, an inline TOML table where the installer requires a regular
table, a scalar value anywhere in the installer-owned trust namespace, and
managed-asset metadata errors other than absence require
`repair_configuration`. A missing `hooks` object remains reinstallable, and a
handler with an exact installer-owned command identity is safely replaced even
when its `type` field drifted. All enum values use snake_case on the wire.

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
| `discovery` | Overlay CLI/state/host discovery failures. |

Canonical public codes currently emitted include:

| Class | Codes |
|---|---|
| `configuration` | `paths_unavailable`, `netbird_configuration_invalid`, `overlay_registry_invalid`, `invalid_discovery_options`, `agent_config_dir_invalid`, `integration_path_untrusted`, `integration_trust_conflict` |
| `daemon` | `version_mismatch`, `method_not_found`, `bad_request`, `daemon_unreachable`, `remote_daemon_unavailable`, `host_governance_unavailable`, `session_input_wait_contract_mismatch`, `projects_not_configured`, `serialize_failed`, `json_error`, `project_task_panicked`, `doctor_task_panicked`, `assistant_materialize_task_panicked`, `assistant_method_unsupported`, `attach_self_feedback`, `daemon_shutting_down` |
| `transport` | `framing`, `host_unreachable`, `request_timeout` |
| `discovery` | `<overlay>_cli_missing`, `<overlay>_state_unavailable`, `<overlay>_listener_address_missing`, `overlay_discovery_failed`, `overlay_peer_collision`, `overlay_host_ambiguous`, `overlay_host_unavailable`, `overlay_error`, `host_unknown`, `remote_discovery_failed` |
| `runtime` | `agent_binary_missing`, `agent_profile_not_found`, `invalid_profile`, `agent_not_resumable`, `agent_native_reference_missing`, `not_resumable`, `invalid_session_ref`, `no_capable_agent`, `bundle_unavailable`, `assistant_bundle_mismatch`, `materialization_failed`, `agent_cannot_read_bundle`, `session_not_found`, `session_not_running`, `session_not_terminal`, `session_external_read_only`, `session_exit_timeout`, `session_runtime_commit_stale`, `session_runtime_conflict`, `session_runtime_reconnecting`, `runtime_supervision_unavailable`, `runtime_supervision_ambiguous`, `runtime_identity_mismatch`, `migration_manifest_missing`, `attach_not_found`, `attach_expired`, `worker_attach_stream_failed`, `worker_protocol_incompatible`, `worker_controller_busy`, `worker_identity_mismatch`, `worker_invalid_state`, `worker_invalid_request`, `worker_invalid_data_token`, `worker_write_outcome_unknown`, `worker_runtime_fault`, `client_file_descriptors_exhausted`, `system_file_descriptors_exhausted`, `pty_alloc_failed`, `spawn_failed`, `pty_error`, `io_error`, `project_store_error`, `project_detect_failed`, `not_a_git_repo`, `project_not_found`, `project_ambiguous`, `prompt_not_found`, `template_not_found`, `action_not_found`, `invalid_name`, `invalid_template`, `invalid_action`, `path_escape`, `config_read_failed`, `agent_not_installable`, `agent_config_dir_missing`, `integration_settings_invalid`, `integration_io_failed`, `worktree_store_error`, `worktree_path_conflict`, `invalid_base_branch`, `worktree_branch_in_use`, `worktree_add_failed`, `invalid_branch`, `invalid_branch_slug`, `notifications_not_configured`, `notification_task_panicked`, `notification_store_error`, `notification_not_found`, `invalid_notification_transition`, `invalid_notification_metadata`, `invalid_notification_session_id`, `invalid_notification_dedupe_key`, `notification_kind_disabled`, `invalid_notification_timestamp`, `invalid_notification_cursor`, `invalid_notification_policy`, `integration_install_in_progress`, `integration_destination_collision`, `integration_recovery_required` |

Protocol v4 emits these runtime codes for provider-neutral agent and
observation behavior: `agent_kind_unsupported`,
`agent_fork_unsupported`, `session_terminal_unavailable`,
`session_has_no_managed_terminal`, `session_runtime_changed`,
`session_read_ansi_unavailable`, `session_output_limit_exceeded`,
`session_wait_limit_exceeded`,
`session_waiter_limit_reached`, `worker_feature_unavailable`, and
`plugin_self_target_denied`, `agent_runtime_unsupported`,
`session_input_rejected`, `session_input_blocked`, `session_agent_blocked`,
`session_input_invalid_wait`, `session_input_wait_unsupported`,
`session_input_timeout`, and the CLI-local `session_input_interrupted`. Daemon startup may additionally return
`observation_limits_invalid`. `runtime_not_installed` is emitted when a runtime
id resolves to no enabled runtime: a host profile whose `base` names an
uninstalled runtime, and resume, fork or mutation of a session whose runtime is
not installed or no longer matches its recorded launch binding. A bare
`session.new` agent name that is neither a profile nor an installed runtime
stays `agent_profile_not_found`. `runtime_incompatible` is emitted when a
runtime package is installed but its root is missing, modified or contradicts
the session's pin, for fresh launch, resume, fork and integration changes (the
message names only the runtime id). `agent_profile_revision_stale`,
`agent_profile_revision_invalid` and `agent_profile_revision_unavailable` are emitted only by the relay-selected launch
source (see above), which no wire method reaches yet.
Observation request errors intentionally carry no terminal
payload or current-runtime payload; refresh `session.inspect` or restart
observation from a fresh screen/tail when recovery requires new coordinates.

`session_input_wait_contract_mismatch` means the SDK cannot prove whether a
same-version daemon honored the wait contract. Delivery outcome is unknown: do
not retry blindly. Upgrade daemon and client together, inspect the session, and
only resend when the observed terminal state proves the original input was not
applied.

`session_input_timeout` also treats delivery as potentially unknown. Inspect the
current session before deciding whether to resend, and do not retry blindly.

`session_input_interrupted` means SIGINT or SIGTERM interrupted a waited CLI
input after delivery may have begun. Its JSON error intentionally has no retry
hint because the delivery outcome is unknown; inspect the session before taking
any further action.

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

Governance inspection also preserves the structured error taxonomy. A daemon
that cannot safely open, revalidate, or reload its owner-private host state
returns a typed error rather than publishing a guessed snapshot. Error messages
and debug rendering are redacted: they do not export approval signing material,
proposal nonces, signatures, or private record bytes. Clients must treat an
inspection failure as unavailable state and must not synthesize an owner,
revision, or enrollment from an earlier response.

### Delegated Task Errors

The delegated task layer (`docs/design/delegated-task-runs-rfc.md` section
13.1) defines the codes below. They are part of the public contract; the
daemon raises them once the task methods are served (until then task methods
answer `method_not_found`). Every one has a fixed `msg` and, where the table
says `yes`, a fixed `recover` hint; `task_snapshot_retired` and
`task_review_limit_reached` carry no hint. Task errors never echo a prompt,
answer, path, task id or secret. The text is fixed in the daemon's response;
the Rust SDK's remote path (`ClientError::RemoteProtocol`) prepends
`host '<host>':` to `msg` and leaves `class`, `code` and `recover` unchanged.
`ProtocolError` has no data field, so `worktree_in_use` and `task_worktree_busy`
cannot name the users or the blocking task; the method slice decides where that
data is carried. Session methods
raise `worktree_busy`, `task_session_ended`, `task_fork_unsupported`,
`worktree_in_use`, `worktree_users_changed` and `task_snapshot_retired` for
sessions that belong to tasks.

| Code | Class | Raised by | Meaning | `recover` |
|---|---|---|---|---|
| `task_turn_open` | `runtime` | `task.continue`, `task.answer` | The latest turn is still open, queued, or already resumed. | yes |
| `task_attention_open` | `runtime` | `task.continue` | The latest turn settled `attention` and was not answered. | yes |
| `task_agent_busy` | `runtime` | `task.continue` | The agent is still visibly working on an earlier prompt. | yes |
| `task_worktree_busy` | `runtime` | `task.start`, `task.continue`, `task.answer`, `task.extend` | Another task or a live non-task session occupies the worktree. | yes |
| `worktree_busy` | `runtime` | `session.input`, attach with terminal control, `session.resume`, `session.fork` (`cwd_mode: "same"`), in-place `session.new` | A task occupies the session's worktree; only observation is admitted. | yes |
| `task_worktree_unavailable` | `runtime` | `task.start`, `task.continue` | The shared worktree no longer exists. | yes |
| `task_worktree_mode_conflict` | `runtime` | `task.start` | `worktree_of` combined with `in_place` or `branch`. | yes |
| `task_session_ended` | `runtime` | `session.resume`, `session.fork`, session recovery | The session belongs to an `ended` task; continue with `task.start { worktree_of }`. | yes |
| `task_session_unavailable` | `runtime` | `task.continue`, `task.answer` | The task is `ended` or its runtime is not live. | yes |
| `task_turn_queued` | `runtime` | `task.extend` | The turn is queued behind a re-open window and has no deadline yet. | yes |
| `task_worktree_via_investigate` | `runtime` | `task.start` | An executor `worktree_of` start named an investigate-mode task. | yes |
| `task_turn_ceiling_reached` | `runtime` | `task.extend` | The turn's total open time reached `tasks.turn_open_ceiling_ms`. | yes |
| `task_answer_unsupported` | `runtime` | `task.answer` | A degraded attention whose manifest declares no input for the answer. | yes |
| `task_answer_unverifiable` | `runtime` | `task.answer` | A keystroke answer without `allow_unverified_delivery`. | yes |
| `task_payload_mismatch` | `runtime` | retried `task.start`, `task.continue`, `task.answer` | The resubmitted payload does not match the stored fingerprint; nothing is dispatched. | yes |
| `task_stop_precondition_failed` | `runtime` | `task.stop` | `if_latest_turn` or `require_idle` did not hold; nothing changed. | yes |
| `worktree_users_changed` | `runtime` | `session.remove` | `expected_worktree_users` differs from the current set. | yes |
| `task_attention_stale` | `runtime` | `task.answer` | The named attention is not the current pending one at that revision, or the provider already resolved it. | yes |
| `task_result_unknown` | `runtime` | `task.review`, `task.result` | No result with that `result_id`. | yes |
| `task_snapshot_retired` | `runtime` | `session.diff` with `turn` | The turn's snapshots were retired with the session content. | no |
| `task_cursor_expired` | `runtime` | `task.list`, `task.inspect` | The paging cursor is too old or from another daemon epoch. | yes |
| `worktree_in_use` | `runtime` | `session.remove`, `worktree.remove` | `session.remove`: other active tasks use the session's worktree. `worktree.remove`: a live session uses the worktree. The code is shared; the removal is refused either way. | yes |
| `task_result_pending` | `runtime` | `task.result`, `task.continue` | The turn settled but its checks have not finished. | yes |
| `task_check_unconfined` | `configuration` | `task.start`, `task.continue` | No kernel-enforced check containment and `checks.allow_unconfined` is not set. | yes |
| `task_check_not_permitted` | `runtime` | `task.start`, `task.continue` | A requested check is not enabled or not permitted for the caller's origin. | yes |
| `task_store_full` | `runtime` | `task.start`, `task.continue`, `task.review` | A task store cap would be exceeded. | yes |
| `task_review_limit_reached` | `runtime` | `task.review` | `tasks.max_reviews_per_result` distinct reviewers already hold a verdict on the result. | no |
| `task_waiter_limit_reached` | `runtime` | `task.wait` | The task waiter pool is full; session waits are unaffected. | yes |
| `task_fingerprint_key_missing` | `runtime` | retried `task.start`, `task.continue`, `task.answer` | The key version a stored fingerprint names is unavailable; nothing is compared or dispatched. | yes |
| `task_request_conflict` | `runtime` | every idempotent task method | A reused request key with different parameters; nothing is executed. | yes |
| `task_investigate_no_checks` | `runtime` | `task.start`, `task.continue` | `checks` or `checks_baseline` requested for an investigate-mode task. | yes |
| `task_fork_unsupported` | `runtime` | `session.fork` | The task's agent cannot fork its native session (OpenCode). | yes |
| `task_investigate_unsupported` | `runtime` | `task.start` | The profile cannot enforce investigation mode. | yes |
| `check_cleanup_stuck` | `runtime` | `task.start` with `worktree_of` | A daemon-owned check process scope in the worktree cannot be confirmed empty; the worktree stays occupied. | yes |

`task_check_unconfined` is the only `configuration` code: the request is
valid and only the owner's `checks.allow_unconfined` opt-in admits it. The
other codes are state conflicts, limits, or refusals of the caller's request.

A queued turn that settles `cancelled` reports the reason `task_turn_reopened`,
`task_stopped` or `session_ended` in its result through `task.wait` and
`task.result`; those are result reasons, not error codes. `task_settled` and
`task_attention` are notification kinds.

Clients must not parse `msg`. Branch on `class` and `code`, then display `msg`
and `recover` for unknown codes.

## Events

`subscribe` turns the control connection into a one-way event stream after this
ack:

```json
{"v":4,"id":"sub-1","ok":{"subscribed":true}}
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
| `session_native_recovered` | `{session: SessionInfo, previous_worker_instance_id?: string, worker_instance_id?: string}` | Explicit provider-native recovery created a new worker and runtime generation for the same logical session. `previous_worker_instance_id` can be absent for a one-time migrated legacy session; production worker recovery includes the new `worker_instance_id`. |
| `agent_state` | `{session_id: SessionId, activity: AgentActivity, source: StateSource, runtime?: SessionRuntimeIdentity, activity_epoch?: string, revision?: ActivityRevision}` | Agent activity changed. `source` may be `report` when a hook report supplied explicit active-agent state. Current daemons emit `runtime`, `activity_epoch`, and decimal-string `revision`, making `(activity_epoch, runtime, revision)` exact reconnect-safe evidence rather than a hint to re-read only the latest snapshot; the fields remain additive for general v2 subscribers, while input-wait success requires them through `SessionInputResult`. |
| `subagent_state` | `{session_id: SessionId, subagent: SubagentInfo, runtime?: SessionRuntimeIdentity}` | One provider-managed subagent changed. Current daemons emit `runtime`; clients ignore an event without a runtime identity or whose runtime id/generation does not match the session snapshot, then apply only a newer decimal-string `subagent.revision` for the same provider/id. `session.list` and `session.inspect` remain the reconnect seed through `SessionInfo.subagents`. |
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
  "protocol": {"minimum": 4, "maximum": 4},
  "ok": {}
}
```

A typed operational or usage failure exits non-zero and replaces `ok` with the
standard `err` object. Usage failures retain exit code 2. No human text is
mixed into stdout. Session output bytes are never logged; non-JSON
`session output` decodes standard base64 and displays UTF-8 lossily, while JSON
preserves the exact `SessionOutputResult`.

`pohunek host governance inspect <host> [--json]` is the CLI surface for the
read-only v4 governance snapshot. Human output labels the stable `HostId`,
approval-key reference, never-enrolled absence, enrollment relay/status/revision,
tagged principal-or-team owner and owner revision, and quarantine. `--json`
emits only the public `HostGovernanceStatus` result through the normal CLI
envelope. The command has no mutation sibling; it does not enroll, transfer an
owner, unenroll, or expose approval or transfer secrets.

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

## Agent Skill CLI

`pohunek agent-skill` prints the complete bundled agent skill for coding
agents. The skill bytes are embedded in the CLI binary at compile time, so one
binary version always prints identical output. The command is a local CLI
lifecycle, not a daemon public method: it never contacts a daemon, reads the
filesystem, or touches the network.

Default output is the embedded skill verbatim on stdout, ending with the
artifact's single trailing newline; stderr stays silent. With `--json` the
command emits exactly one pretty-printed CLI process envelope whose `ok`
payload carries the skill text and its lowercase-hex sha256 content hash:

```json
{
  "cli_version": "0.x.y",
  "protocol": {"minimum": 4, "maximum": 4},
  "ok": {
    "skill": "<complete skill text>",
    "content_sha256": "<64 lowercase hex characters>"
  }
}
```

The global `--host` flag is accepted and deliberately ignored. The skill is
embedded locally, so no host is resolved, probed, or connected to, and local
and remote invocations print the same document.

## Hermes Operator Plugin CLI

The Hermes operator plugin is a local CLI lifecycle, not a daemon public method.
Historically, M3 did not bump the then-current public protocol v2 or add a
Hermes-specific wire shape. Current builds use public protocol v4. The CLI
embeds the plugin assets and generated skill, then installs them only into an
explicitly selected Hermes profile or custom absolute home.

```bash
pohunek integration install --agent hermes --hermes-profile default \
  --access-mode manage --allow-host local \
  --tool-timeout-ms 8000 --max-output-bytes 262144 \
  --max-screen-bytes 65536 --max-concurrency 1 --json
pohunek integration doctor --agent hermes --hermes-profile default --json
```

`--hermes-profile default`, a named `--hermes-profile`, and an absolute
`--hermes-home` are explicit target selections; a profile and home cannot be
combined. `update` is Hermes-only and returns
`configuration/integration_action_unsupported` for another agent; `doctor`
and `uninstall` also serve Codex and Claude (`doctor` follows `--host`,
`uninstall` targets the local daemon and requires `--agent`). Hermes status
uses the same local target contract; daemon-backed Codex/Claude status uses the
new RPC without those local Hermes flags, and their install behavior is unchanged:

```bash
pohunek integration status --json
pohunek integration status --agent codex --json
pohunek integration status --agent claude --json
pohunek integration status --agent hermes --hermes-profile default --json
pohunek integration doctor --agent codex --json
pohunek integration uninstall --agent claude --json
```

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
`session.input`. The plugin exposes no consent removal, so it has no counterpart for the daemon's denial of `session.remove_accepting_unconfirmed`. Exactly three lifecycle reports may target the origin:
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

The attach byte stream is part of public protocol v4. It is not an implementation
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
- On the WebSocket relay transport, the attach prelude is
  sent as the first bytes on the `/daemon/<host>/attach` binary WebSocket. After
  redemption, every binary frame remains opaque PTY data.

Rust SDK helpers:

- `attach_raw(host, socket_path, stream_id)`
- `attach_raw_local(socket_path, stream_id)`
- `attach_raw_tcp_addr(host, addr, stream_id)`
- `*_with_options` variants

These helpers open the raw connection and write the prelude before returning a
`RawStream`.

## External clients

The web control center and the native desktop GUI live in
[`zajca/pohunek-work`](https://github.com/zajca/pohunek-work) and are external
clients of this protocol and the SDKs; nothing in them is private. They pin
core by git tag (Rust crates) and by release-tarball URL and integrity (the
TypeScript SDK), and move in lockstep with the protocol version: the TypeScript
SDK handshake requires the daemon's exact `PROTOCOL_VERSION`. Core crates beyond
`pohunek-client` that a Rust client links are a pinned, not a stable, API with no
back-compat shims. Three methods
are public obligations whose callers are mostly those external UI clients, so
core keeps server-side contract tests for each of them: `host.discover` and
`worktree.remove` are called only by the UI clients, and `subscribe` is also
called by the CLI (`pohunek subscribe`, `pohunek attach`, `pohunek notifications`).

- `host.discover`: socket-level coverage in
  `crates/daemon/tests/health_socket.rs`
  (`public_bind_serves_host_discover_with_supplied_registry`) and the
  version-mismatch contract test in `crates/daemon/tests/remote_tcp.rs`.
- `subscribe`: socket-level coverage in `crates/daemon/tests/health_socket.rs`
  (`subscribe_streams_session_created_event` and the notification and
  agent-state subscribe tests) and the client subscription tests in
  `crates/client/tests/subscription.rs`.
- `worktree.remove`: socket-level coverage in
  `crates/daemon/tests/health_socket.rs`
  (`worktree_remove_over_the_socket_succeeds_and_fails_closed`): success after
  stop, `worktree_in_use` for a live session, `worktree_not_owned` for the main
  checkout, and `bad_request` for malformed params.

## Rust SDK Surface

The `pohunek-client` crate is the supported Rust client surface. New Rust
clients should use it rather than hand-writing protocol framing.

Public exports:

- `protocol`: re-export of `pohunek-protocol`.
- `Client`: framed request/response and subscription client.
- `ClientOptions`: `request_timeout` and `connect_timeout`, both defaulting to
  5 seconds, and `origin_source`, set with `with_origin_source`.
- `OriginSource`: where a connection takes its request origin from.
  `Environment` (the default) reads `POHUNEK_SESSION_ID` and
  `POHUNEK_DAEMON_ID` from the calling process and rejects a partial or invalid
  pair; `Omitted` attaches no origin of the connection's own and never reads
  the environment (a request built with an explicit origin keeps it).
  `DiscoveryOptions::with_origin_source` selects it for discovery probes.
- `Subscription`: raw event-line stream after a successful `subscribe`.
- `RawStream`: local Unix or remote TCP raw byte stream for attach.
- `ClientError`: SDK error enum with `to_protocol_error()` for structured
  rendering. An elapsed response deadline maps to `request_timeout`, distinct
  from connection and daemon-discovery failures; the timed-out mutation may
  still have completed remotely.
- `next_request_id(method)`: shared correlation-id generator used by SDK-backed
  clients.
- `remote_host_with_port(host, port)`: formats a provider-resolved selector
  with an explicit non-zero daemon port.
- `ConfiguredTransport` and `OverlayRegistry`: validated provider registry with
  stable overlay IDs and one non-zero daemon port per provider.
- `OverlayTransport`: provider contract whose required typed identity resolver
  must match only the requested peer-ID or FQDN field; implementations cannot
  inherit an untyped selector fallback.
- `discover_hosts(registry)`: aggregated configured-overlay discovery with
  default bounded probes.
- `discover_hosts_with_options(registry, options)`: the same discovery with an
  explicit per-probe timeout, overall deadline, and concurrency bound. Provider
  failures are isolated unless every configured overlay fails. Providers return
  remote peers only; clients that need a local entry add their Unix-socket target
  explicitly.
- Raw and attach helpers: `connect_raw*` and `attach_raw*`.

Public daemon construction is registry-explicit. `DaemonState::new` and
`ControlServer::bind` require a validated `OverlayRegistry`, and the shared
discovery cache cannot be constructed without one. This keeps `host.discover`
usable through the public server constructor instead of creating a latent
registry-less state.

Connection APIs:

- `Client::connect(host, socket_path)`: `""` and `"local"` use the Unix socket;
  any other host is resolved through the default configured overlay registry
  and dialed over its exact per-overlay route. `<overlay>:<selector>` restricts
  resolution to one configured provider. The optional
  `<overlay>:<selector>@<port>` form retains an explicit discovered daemon
  port while still resolving the selector through current provider state. The
  `@` character is reserved by this route grammar. Generated stable selectors
  use `peer~<base64url>` or `fqdn~<base64url>` without padding; the registry
  decodes the value and preserves its identity kind before provider resolution.
  Providers must implement that typed resolution explicitly and cannot reinterpret
  an FQDN as a peer ID or short name. This keeps raw provider IDs containing `/`,
  `+`, `=`, or `@` out of target grammar. A bare IPv6 literal
  remains an unqualified selector; only an explicit configured-overlay prefix
  qualifies it. A socket-address literal is not a bypass and must resolve under
  current provider policy.
- `Client::connect_with_registry(host, socket_path, registry)`: the same routing
  with a caller-supplied registry; ambiguous names fail closed.
- `Client::connect_local(socket_path)`: direct Unix socket.
- `Client::connect_trusted_tcp_addr(host, addr)`: direct TCP for a route already
  validated by configured overlay discovery, with host context preserved for
  remote errors.
- `*_with_options` variants accept `ClientOptions`.
- `Client::attach_raw(stream_id)`: opens the raw attach connection on the exact
  endpoint selected for that client, without re-resolving the peer.

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
- `Client::session_detection(SessionDetectionParams)`: requests current active
  manifest-region previews from the live detector task.
- `Client::session_read(SessionReadParams)`: reads bounded current-screen text on
  the current connection.
- `Client::session_output(SessionOutputParams)`: uses the current connection for
  an immediate read and automatically opens a dedicated connection when
  `wait_ms` is present.
- `Client::session_input(SessionInputParams)` and generic typed
  `Client::call::<SessionInput>`: use a dedicated connection when
  `wait` is present, budgets the wire timeout as the daemon's overall
  delivery-and-wait deadline plus fixed response headroom, and rejects successful
  responses that omit epoch- and runtime-scoped activity evidence.
- `Client::session_wait(SessionWaitParams)`: automatically opens a dedicated
  connection for the bounded long poll.
- `Client::host_governance_inspect()`: calls `host.governance.inspect` with
  exact null parameters and returns the safe `HostGovernanceStatus` projection.
  It has no local cache or mutation behavior.
- `Client::session_resume`, `session_resize`, and `session_set_metadata`: typed
  lifecycle helpers used by automation clients.
- `Client::integration_status(IntegrationStatusParams)`: reads the complete
  daemon-managed Codex/Claude install contract without mutation.
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

Distribution: the TypeScript packages are not published to an npm registry.
Each release attaches three npm-pack tarballs, `pohunek-ts-protocol-X.Y.Z.tgz`,
`pohunek-ts-sdk-X.Y.Z.tgz` (the `@pohunek/sdk` package) and
`pohunek-ts-testkit-X.Y.Z.tgz`, each with a `.sha256` file in the same format as
the other release checksums. A consumer pins the release asset URL in its
`package.json`, for example
`"@pohunek/sdk": "https://github.com/zajca/pohunek/releases/download/vX.Y.Z/pohunek-ts-sdk-X.Y.Z.tgz"`,
and the lockfile records the tarball integrity. Inside each tarball every
`@pohunek/*` dependency is rewritten to the exact release-asset URL of the
sibling package for the same tag, so the closure resolves from one release and
the `@pohunek` scope is never looked up on a registry. `devDependencies`,
`scripts` and `private` are dropped from the packed manifests. Each tarball
ships compiled ES modules under `dist/`, matching declarations under `types/`,
and (for `@pohunek/protocol`) the `fixtures/*` data files; no TypeScript source
is shipped. The packed `exports` map points every entry at `./dist/*.js` with
a `types` condition at `./types/*.d.ts`, so the packages load under Node without
a loader or bundler and type-check under both `moduleResolution: "bundler"` and
`"nodenext"`. The packages have no third-party runtime dependencies. Within the
repository workspace the manifests keep pointing at `src/*.ts` for the Bun dev
loop; only the packed artifact is compiled, by
`sdk/ts/scripts/build-package.ts`. `@pohunek/testkit` is shipped compiled the
same way. Its root entry uses no Bun globals and runs on Node >= 20 and Bun; the
explicit `@pohunek/testkit/bun-relay` subpath needs `Bun.serve` and is Bun-only
(under Node it loads but `startTestRelay` fails fast, naming the subpath). The `sdk/ts/scripts/test/pack-contract.test.ts` contract test
installs the packed tarballs by URL against an unreachable registry, imports
every entry point under Bun and under the Node binary named by
`POHUNEK_TEST_NODE_BIN`, type-checks a consumer with both resolvers, and checks
that the browser entry reaches no `node:` module. It also starts the installed
`@pohunek/testkit/bun-relay` under Bun and tunnels a real WebSocket to a real
Unix-socket server, and asserts the root testkit entry does not expose the
relay. CI and the release SDK gate run
it under Node 20 and Node 22.

Public exports:

- `Client`: framed request/response and subscription client.
- `nextRequestId(method)`: shared correlation-id generator used by SDK-backed
  TypeScript clients.
- `SocketTransport`: direct Unix/TCP `node:net` transport for Bun/Node; exported
  only by the root `@pohunek/sdk` entry.
- `WsTransport`: transparent WebSocket relay transport using the
  WHATWG `WebSocket` global.
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
  event-name constants. Generated files under `sdk/ts/protocol/src/generated/**` are
  refreshed only by `cargo xtask ts generate`, never hand-edited.

Supported runtimes:

- Bun: supports the direct socket transport and the WebSocket relay
  transport.
- Node >= 20: supports the direct Unix/TCP socket transport through `node:net`.
  Node 18 is not supported: the SDK reads the `globalThis.crypto` global, which
  Node exposes without a flag only from Node 19.
- Node >= 22: supports the WebSocket relay transport through the
  built-in WHATWG `WebSocket` global.
- Browser: import `@pohunek/sdk/browser`; it supports only the WebSocket
  relay transport because browsers cannot dial daemon Unix sockets or
  NetBird TCP directly.

Connection APIs:

- `Client.defaultOptions()`: returns resolved default timeouts.
- `Client.connectWs(baseUrl, host, opts?)`: WebSocket relay transport.
  `baseUrl` may use
  `http`, `https`, `ws`, or `wss`; the SDK connects to
  `/daemon/<host>/control` under that base URL.
- `Client.connectTransport(transport, opts?, remoteHost?)`: injection point for
  tests and custom transports that implement `Transport`.
- `connectLocal(socketPath, opts?)` and `connectTcp(host, {host, port}, opts?)`:
  root-entry Bun/Node helpers for a direct Unix socket or daemon TCP address.
- `SocketTransport.unix(socketPath, opts?)` and `SocketTransport.tcp(host,
  {host, port}, opts?)`: construct direct socket transports.
- `WsTransport.relay(baseUrl, host, opts?)`: constructs the WebSocket
  relay transport for `/daemon/<host>/control` and
  `/daemon/<host>/attach`. The method name predates the accepted team relay.

Request APIs:

- `client.call(method, params)`: typed call keyed by the generated `Methods`
  map. A configured origin is added to the wire request.
- `client.call("host.governance.inspect", null)`: the generated method map
  returns `HostGovernanceStatus` only after runtime validation of the exact six
  required fields, canonical opaque IDs and revisions, known lifecycle and
  quarantine values, and the enrollment/owner/quarantine invariants. A missing,
  unknown, malformed, or inconsistent result fails closed as the redacted,
  SDK-originated `daemon/host_governance_inspect_contract_mismatch` error; it
  never includes the raw response payload. The TypeScript SDK deliberately has
  no governance mutation convenience API and exports only the public-safe result
  types.
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
  explicit address to `attachRawTcp` or use a WebSocket relay with
  `attachRawWs`.
- Root-entry `attachRawLocal` and `attachRawTcp`, plus shared `attachRawWs`, open
  a raw channel, write exactly one attach prelude, parse a failed redemption
  response as `ClientError`, and otherwise return the raw attach stream.

SDK error mapping:

- Daemon protocol errors are preserved as `ClientError.kind === "protocol"` or
  `"remoteProtocol"` and retain the daemon's original `class`, `code`, `msg`,
  and `recover` fields, except for the local TypeScript decoder's
  `daemon/host_governance_inspect_contract_mismatch` result. That redacted
  contract-mismatch error is SDK-originated even though it uses the normal
  structured protocol shape.
- Most SDK-originated errors map into the public protocol taxonomy through
  `ClientErrorClass` and `ClientErrorCode`: `daemon_unreachable`, `framing`,
  `host_unreachable`, `remote_daemon_unavailable`, `request_timeout`, `io_error`,
  `json_error`, `session_input_wait_contract_mismatch`, and `version_mismatch`.
  `host_governance_inspect_contract_mismatch` is the separate fixed, redacted
  SDK-originated governance-response validation error.
- `ClientError.toProtocolError()` returns the structured `ProtocolError` for
  CLI/API rendering, and `recoverHint()` returns the optional recovery text.

The Bun-only `@pohunek/testkit/bun-relay` subpath (not the root entry, which
also runs on Node) exports the loopback-only test relay used by the SDK
transport tests:
`startTestRelay({bindHost, port, targets})` returns a `TestRelayHandle`
(`url`, `port`, `close()`), with the `DaemonTarget`, `DaemonTargetSource`, and
`StartTestRelayOptions` types. `bindHost` must be a loopback address; any other
bind fails. The helper implements the WebSocket framing documented above as a
transparent 1:1 tunnel and is not a deployable relay or `pohunek-relayd`.
