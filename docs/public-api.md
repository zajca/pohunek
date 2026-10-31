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

The current public protocol version is `4` (`PROTOCOL_VERSION`). A daemon of this
build accepts the inclusive range `3..=4` (`SUPPORTED_PROTOCOL_VERSIONS`): its own
version and the previous one, which it serves through shape-only edge adapters.
The Rust client, the CLI and the TypeScript SDK of this build advertise the same
window (`CLIENT_PROTOCOL_VERSIONS` is `SUPPORTED_PROTOCOL_VERSIONS`), so they also
talk to a daemon of the previous release
([#527](https://github.com/zajca/pohunek/issues/527)).
Requests carry `v: {minimum, maximum}`. The first valid response selects the
highest overlapping version as an integer `v`, and that selection is fixed for the
lifetime of the connection. Subscription events use the same selected version and
are shaped for it. A non-overlapping range returns `daemon/version_mismatch`.
`daemon.health` and `host.inspect` report the version negotiated for the asking
connection in `protocol_version`.

A protocol 3 connection is answered `method_not_found` for any method added after
protocol 3 (today the `package.*` methods), and never receives their results.
The `session.list` metadata predicates `metadata_eq` and `metadata_has` are also
current-protocol-only request shapes. Because an older daemon knows the method
but cannot apply these predicates, current Rust and TypeScript clients probe
the negotiated version and return `daemon/daemon_protocol_too_old` before
sending the filtered request.

The CLI exposes these predicates as repeatable `session list --meta-eq
key=value` and `--meta-has key` options. They combine with `--filter` and each
other using AND semantics; equality splits at the first `=` and opaque keys or
values may be empty. A CLI targeting a protocol-3 daemon fails before sending
a metadata-filtered request, while an unfiltered `session list` remains
available.

A Rust client, CLI or TypeScript SDK client whose connection selected protocol 3 (a daemon of the previous
release, typically on another host) translates in the opposite direction through the
same adapter: it sends requests in the protocol 3 spelling (`runtime_id`, pinned to
`v: {3,3}`) and hands its caller the current shape of every result and event, so
`--json` output and `Subscription::next_line` never carry `runtime_id`. A request
whose shape depends on the version first sends `daemon.health` on the connection,
offering only the versions the request itself allows; a connection whose selected
version lies outside the request's own range fails client-side before anything is
sent (`daemon_protocol_too_old` below the range, `version_mismatch` above it).
After a version is selected every request is pinned to it, except `host.discover`
(`RANGE_FORWARDING_METHODS`): the daemon classifies peers for the range the request
advertised, so the client keeps its whole window on it and a protocol 3 peer stays
`reachable_daemon` however many requests the connection has already served. A
method added after protocol 3 is refused before it is sent, with the typed
`daemon/daemon_protocol_too_old` error. Its message names the host, the protocol the
daemon runs and the protocol the method needs ("host 'netbird:build-2' runs protocol 3,
but package.list needs protocol 4"); its `recover` hint says to upgrade pohunek on
that host. A payload that cannot be translated is refused with
`daemon/version_translation_failed`. A runtime reference that is not one of the
protocol 3 agent kinds (`shell`, `codex`, `claude`, `hermes`) is not checked on the
client: the protocol 3 daemon answers it with its own typed
`agent_kind_unsupported`.

The window is bounded. Adapters translate shape only (renames and moved fields,
and withholding events and enum values the older version never defined). A
semantic change (a new required parameter, a removed method or event, a changed
error code or meaning) cannot be adapted: it raises the accepted minimum as well
and is announced as a break in the release notes. The next protocol bump deletes
the oldest adapter, so each release accepts exactly its predecessor and no more.
See `docs/architecture.md` "Protocol versioning".

Within one protocol version a daemon of a newer build can answer with an enum
value an older Rust client does not know. The Rust client and the CLI decode
daemon responses and events tolerantly: `ErrorClass` has a hidden variant
`Unknown`, which prints as and serializes to `"unknown"`, so an error with a class
the client does not know still reports the daemon's `code`, `msg` and `recover`,
and `--json` emits `{"class": "unknown", ...}`. The daemon never emits `unknown`
and rejects it in params. The TypeScript types do not list the variant. The
per-enum policy is in `docs/architecture.md` "Unknown enum values from a newer
daemon".

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
agent-runtime meaning. Protocol 4 has no `runtime_id` alias for the worker
instance. A v3 client that sends its range `3..=3` to a v4 daemon is served by
the protocol 3 adapter: the daemon accepts the `runtime_id` spelling in its
requests (a request that already carries the v4 spelling is `bad_request`) and
answers, and streams events, in the `runtime_id` spelling, withholding warning
kinds and events v3 never defined (a v3 connection never receives a
`native_recovery` session warning; v4 connections do). The `background_state`
event is withheld from v3 connections, and `SessionInfo.background` is removed
from the session objects a v3 connection receives, so a v3 client sees no
background snapshot. `SessionInfo.capability_refusals` and `SessionRuntime.exit` are removed from
them as well. A client whose range does not overlap the window
receives `daemon/version_mismatch` before any method runs; a client of this build
negotiates v3 and translates as described above. The window covers
the public daemon protocol and the Hermes CLI tool path: the CLI's `--json`
envelope reports its actual `3..=4` window and its session summaries carry
both `worker_instance_id` and the v3 `runtime_id` spelling. Other typed CLI
results retain their current shape. Its observation
commands accept the old `--runtime-id` spelling as a hidden alias while v3
remains in the window. A v0.33.0 Hermes plugin whose policy pins `3..=3` can
therefore keep using tools, including timed-out start recovery, without
rewriting its owner policy. A policy outside the window fails closed with
`pohunek_cli_incompatible`; `integration update --agent hermes` refreshes
the plugin assets and policy explicitly. A client and a
daemon more than one release apart are refused.
Notification hooks deliver through the worker socket when
`POHUNEK_WORKER_SOCKET_PATH` is set and fall back to the daemon socket otherwise;
the worker's `notification_create` and `prompt_accepted` hook requests are additive to the private
worker protocol, so a worker that predates them simply refuses them. Managed hook assets carry `POHUNEK_INTEGRATION_VERSION=15`; an asset of
an earlier version still sends the old key, which the daemon accepts only from a
session whose launch baked protocol 3 (and an earlier notification hook still
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
| Remote | One TCP listener per configured overlay, bound to that provider's validated local member address and port, which must also be the address the provider reports at bind time. The provider and the port come from the owner-private `overlays.toml` (see "Overlay configuration" below); without the file the host works over NetBird on port 18722 | Overlay reachability and provider policy. NetBird is the default provider and Tailscale a supported opt-in provider. The listener authenticates nothing beyond overlay reachability, so the overlay's policy (NetBird policies, tailnet ACLs) is the access control |

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
  fail-closed to the owner overlay (for NetBird and Tailscale, an address in
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
while this public fallback admits only the session's PTY root itself, attested
by the kernel peer of its local connection, refuses every report for an
`identity-v1` session with `native_report_unattested`, and then applies its
runtime, ordering, expiry, and provider rules. This is a narrow server-side confused-deputy guard
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
| `daemon.doctor` | `null` | `DaemonDoctorResult` | Runs daemon-local checks. Non-null params are `daemon/bad_request`. Each check is `{name, status, detail}`: `name` is the stable code and `detail` carries the remediation. Both platforms report `worker_executable` for the active supervision's worker, including a bounded capability probe of its readable journal schema and overlapping private protocol range; an older worker without capability reporting is `warn` (unverified), while a missing, unsafe, malformed or incompatible required worker is `fail`. Linux also reports `bin:git`, `bin:codex`, `bin:claude`, the socket, state and log directory writability checks, one `<overlay>_cli` check per configured overlay (`netbird_cli`, `tailscale_cli`) and `schema_version`; macOS adds `runtime_dir_private`, `socket_path_length`, `filesystem_access`, `login_shell`, `launchd_domain`, `desktop_notifications` and `keychain`. It also reports configured profile checks as `agent_profile_<name>`, optional provider checks as `agent_runtime_codex`, `agent_runtime_claude` and `agent_runtime_hermes`, embedded manifest checks as `agent_manifest_embedded_<runtime>` for the runtime compiled in (`agent_manifest_embedded_shell`; Codex, Claude Code and Hermes ship only as packages, so no embedded Codex, Claude or Hermes manifest is checked), and host override checks as `agent_manifest_override_<name>`. Configured profiles fail when they cannot resolve securely, their package runtime is unavailable, their executable is missing, or the provider's existing pinned version rule rejects it. An absent unconfigured optional provider warns. Embedded manifests are parsed from the daemon's canonical sources; host overrides use the same bounded, contained loader and precedence as launch. An invalid referenced override fails, an invalid unreferenced override warns, and an empty override is valid and disables detection. Details are fixed and redacted. The daemon also reports Codex and Claude hook readiness as `agent_integration_<provider>` for runtime homes and `agent_integration_<provider>_profile_<name>` for configured profile homes. Current hooks are `ok`; missing or outdated hooks and an active install lock are `warn`; malformed or unsafe provider state is `fail`. A local request checks each distinct launch home using the same integration inspector as `integration.doctor`; a remote request checks runtime homes only, preserving the local-only boundary for profile-derived homes. Configured Hermes profiles receive `agent_integration_hermes_profile_<name>` with `warn` and an explicit unverified detail because their hook lifecycle is CLI-managed and the daemon has no config-home inspection policy. No home path, profile environment value, provider config text or parser output is returned. `<overlay>_cli` is `ok` with the address the provider reports for this host, `warn` when the provider's state is unavailable or reports no address, and, when the provider CLI is missing or untrusted, `fail` for an overlay that `overlays.toml` lists and `warn` for the NetBird default of an absent file. The daemon also reports one `overlay_<overlay>_listener` check per configured overlay (`overlay_netbird_listener`, `overlay_tailscale_listener`): its detail starts with `port=<n>;` and states the listener address the provider currently reports, which is the provider's state and not proof that the daemon has bound it; `warn` when the provider reports none. A daemon that predates these checks reports none. `metadata_store_rejected_lines` is `ok` when the metadata store holds no line this build cannot load, `warn` with the count (and how many may pin a package) and the command `pohunek store rejected list` otherwise, and `fail` when the store cannot be read; it never carries a line's text. `catalog_trust_anchor` is `ok` when the release's catalog trust anchor is loaded, `warn` when the installation ships none, and `fail` when it cannot be trusted. Hook interpreter readiness (`python3`) is reported by `integration.doctor`, not here. `overall` is `fail` only for required failures. A check status a newer daemon defines reads as `unknown` on the Rust client and the CLI, ranks like `warn` when checks are merged (never `ok`), and keeps the daemon's other checks visible. |
| `host.inspect` | `HostInspectParams` or `null` | `HostCapabilities` | Capability snapshot for the daemon's host. Presence and path of every agent program are resolved on each call; the version probes behind `runtimes[].version` and `supported` come from a probe cache (see "Version probe"), so a result may be older than the cache lifetime. The inventory is built off the daemon's async runtime. `HostInspectParams.skip_runtime_probes: true` answers only the feature flags, `daemon_version` and `git_available`, with empty `supported_agents` and `runtimes`, and resolves and probes no agent program; a client that only gates a request on a capability flag sends it (the CLI does for `session new --cwd`, `--from-remote` and `--input-ack-timeout-ms`). A daemon that predates the parameters ignores them and answers the full inventory, so such a client must accept a complete answer; a client that sends no parameters always gets the full inventory. The Rust method type keeps `()` parameters (the full inventory) and `Client::host_inspect_with` sends `HostInspectParams`; the TypeScript SDK types the parameters as `HostInspectParams | null`. |
| `host.discover` | `HostDiscoverParams` or `null` | `Vec<HostRecord>` | Enumerates peers from configured overlay transports and classifies daemon reachability. |
| `host.resolve` | `HostResolveParams` (`{route}`) | `HostResolution` | Resolves a host route as `--host` accepts it against this daemon's own overlay view: `{target: "local"}` for `local`, the empty route and this machine's own overlay name, or `{target: "remote", address, port, pinned_route, overlay, peer_id?, fqdn?}` for one peer. `pinned_route` (`<overlay>:peer~<id>@<port>`, or `fqdn~` without a peer id) selects the same peer after a name or address change. Same rules and typed errors as `Client::connect` (`host_unknown`, `overlay_host_ambiguous`, `overlay_host_unavailable`, `host_identity_unpinnable`). Served on the local Unix socket only: over the overlay and on a protocol 3 connection it answers `method_not_found`, and a client of this build refuses it against a protocol 3 daemon with `daemon/daemon_protocol_too_old`. |
| `host.governance.inspect` | `null` | `HostGovernanceStatus` | Owner-safe, read-only snapshot of the daemon's stable host identity and local governance state. Explicit `null` and omitted params both succeed; every non-null JSON value returns `daemon/bad_request`. The method never changes enrollment, ownership, or sessions. On unavailable governance it returns the fixed redacted `daemon/host_governance_unavailable` error; reload or restart the daemon, then retry. A `quarantine` reason a newer daemon defines reads as `unknown` on the Rust client and the CLI; the status cross-check still requires a quarantine reason exactly when the enrollment is quarantined. |
| `session.new` | `SessionNewParams` | `SessionNewResult` | Starts an agent PTY session. With initial `input`, a write the agent does not read for the configured stall bound (`[input] write_stall_timeout_ms`, 3000 ms by default) fails with `session_input_stalled`; part of the input may already have been written, and `session.new` then rolls the session back: the create fails with that error and the session is removed, not left running. With `input` and `input_ack_timeout_ms` (`1..=8000`; without `input`, `bad_request`; outside the range, `session_input_invalid_wait` or `session_wait_limit_exceeded`), the daemon delivers the input like a waited `session.input` and reports `prompt_delivered`: `true` when `working` activity followed the submit within the window (counted from the moment the submit was acknowledged and recorded), `false` when it did not, or when the runtime changed, the session ended or the daemon began shutting down first, or the delivery ended after Enter's send began; the session is kept and `applied_input` stays `true` either way. Delivery that fails before Enter's send began, or with `session_input_stalled`, keeps the rollback above. The agent must not report `blocked`: a blocked agent fails the create like any waited input. Clients check `HostCapabilities.session_new_input_ack_supported` first, because a daemon without it drops the field silently; the Rust and TypeScript clients extend the `session.new` response budget by the window plus `MAX_SESSION_WAIT_MS`; protocol 3 peers refuse the field. Before session, worker, or worktree creation, the program a bare or profile-based launch names must resolve to an executable on the daemon's `PATH`, or the call fails with `agent_binary_missing`; this refusal's message and recover hint name only the program's file name. A bare host-shell session is the exception: it launches the configured shell command, which its launch resolves and reports with the same code. A runtime that declares a version probe (runtime packages that declare one) then requires the isolated, bounded probe to accept that executable; a rejected release, unparseable output, a failing or timed-out probe, or a parser this daemon does not compile in returns `agent_runtime_unsupported`, also before session, worker, or worktree creation; its message names the parsed release and the supported range, or that the output did not parse, and never carries probe output (see "Version probe"). With `project` or `repo` and no `branch`, an optional `cwd` is the directory the session starts in: an absolute path of an existing directory on the daemon's host, inside the project checkout or anywhere else; the session starts with the reference's project association (what a later cwd change does to it is described below). A relative path, a missing path or a path that is not a directory fails with `bad_request` naming the path before anything is created; without `cwd` the session starts in the project checkout. With neither `project` nor `repo`, `cwd` selects the project by detection and, when it is an absolute existing directory, is where the session starts, even below the checkout root; the CLI's own default for a start without `--cwd` is the root of the work tree around its directory. With `branch` the session starts in its worktree, and `cwd` only selects the repository when neither `project` nor `repo` is given. With `branch`, the optional `branch_source: "remote"` takes the branch from `origin` instead of the local branch: the daemon fetches `refs/heads/<branch>` into a private ref and, by how the local branch relates to the fetched commit `R`, creates the branch at `R` (no local branch), uses it (equal), fast-forwards it with a compare-and-swap ref update (behind), uses the local head and adds a `branch_ahead_of_remote` warning naming both commits (ahead), or refuses with `branch_diverged` (diverged: the message names the local, remote and merge-base commits and the local branch is left untouched). `on_diverged: "backup_reset"` instead keeps the local head as `refs/heads/backup/<branch>-<first 12 hex digits of its commit>` and resets the branch to `R`, both in one step that applies neither when the branch moved meanwhile (`worktree_add_failed`, the message states the concurrent move); a taken backup name (a ref at that name, or one above or below it) fails with `backup_ref_exists`, unless the ref already holds the local head, which is then reused; a failed update is classified by the refs it left, so a reset that a concurrent identical request applied first succeeds, and any other position is `worktree_add_failed` naming the backup that holds the old head; any later failure of that request (the worktree add, or a branch that moved again before the checkout) names the backup ref in its message, because the old head is reachable only through it. A branch that would move while another worktree or the main checkout has it checked out, or is in the middle of a rebase or bisect that started on it, or is listed in the `update-refs` of a rebase running in another worktree (`git rebase --update-refs` moves the other branches of its stack when it ends), fails with `worktree_branch_in_use` before any ref changes (the check runs again under the repository's worktree lock before the move, which excludes only the daemon's own concurrent worktree adds). A `branch` that git expands rather than names, such as `@{-1}`, fails with `invalid_branch`. A missing `origin`, or an `origin` without that branch (a tag of the name does not count), fails with `remote_branch_not_found`, and any other failed fetch with `branch_fetch_failed`; neither is a warning, and a refused request leaves no worktree, binding or session. `branch_source` requires `branch`, excludes `base_branch`, and `on_diverged` requires `branch_source: "remote"` (`bad_request`); absent or `"local"` keeps the behavior of an existing local branch described below. The result then carries `worktree_source` `{ref, commit, fast_forwarded_from?, backup_ref?}`: `ref` is `origin/<branch>`, or `refs/heads/<branch>` for an ahead branch whose local head was used. The upstream configuration of the branch is not changed. A daemon that predates the options ignores both and checks out the stale local branch, so a client checks `session_new_branch_source_supported` in `HostCapabilities` before it sends either (the CLI and `pohunek assistant` refuse with `session_new_branch_source_unsupported`); a result without `worktree_source` from a daemon that was not checked means the option was ignored; against a protocol 3 daemon the Rust client and the TypeScript SDK also refuse a request that sets either (any value) with `daemon/daemon_protocol_too_old`. A daemon that honors `cwd` for a project-bound session advertises `session_new_cwd_supported` in `HostCapabilities`; one that does not ignores it, so a client checks the flag before it relies on `cwd` (the CLI requires it for every explicit `--cwd`). The daemon stores the canonical form of the directory, and a session started by reference in a directory outside every repository holds its create-time association (the project; `repo` and `branch` are not part of it) in memory: it keeps it while the observed cwd provably lies outside every repository (`git` reports no repository and no parent holds one; also on return from another repository, whose association it follows meanwhile, and across a stop and resume in the same daemon run), and an observed cwd the daemon cannot classify, such as a missing directory or a `git` that fails, clears it. Every other session, including one started inside a repository and any session after a daemon restart, follows its cwd as `session_updated` always did. While startup found legacy resume bindings with no logical record and no migration manifest (listed in `session.runtime_inventory` as `orphaned`, `migration_manifest_missing`), it fails with `migration_manifest_missing` before anything is written, because a first logical record would make a later manifest unimportable; run `pohunek migration preflight` against the legacy daemon, and the next daemon start that imports the manifest lifts the refusal. A session whose worker socket could never be bound (the published `control.sock` path or the longer staged bind path beside it exceeds the platform `sun_path` limit) fails with configuration-class `worker_socket_path_invalid`, naming the path and the limit, before anything is written or launched; `session.fork` applies the same check. A worker job that ends before accepting connections fails the call with `worker_exited_before_ready` as soon as the service manager shows the job ended, carrying the worker's exit status where the supervisor retains it, instead of waiting for the worker connect deadline (`worker_connect_failed`). A launch whose working directory is no longer a directory (a worktree removed after it was bound) is refused by the worker before the agent spawns, and the call fails with runtime-class `session_cwd_missing`; the create is compensated as below, so no agent runs outside its working directory. A create that fails after binding a worktree is compensated once its runtime is proven ended (checkout and binding, then the durable record); a compensation that cannot finish (for example a checkout locked with `git worktree lock`) still fails the call with its original error, keeps the record, and lists the session as `reconnecting` with reason `create_compensation_pending` while the daemon retries the compensation with the supervision backoff, until the session leaves with `session_removed`. A session launched from a host profile freezes the keyed revision of that profile into its resume binding; when the host cannot read or create its revision key the create fails with `agent_profile_revision_unavailable` before anything is written (a session without a profile needs no key). Optional `metadata` is written atomically with the session (see the `metadata` field note under `SessionInfo` below); the CLI exposes it as repeatable `--meta key=value`. A session created with initial input into a runtime whose `[input]` table declares `ready = "screen_idle"` writes that input only once the agent's rendered prompt box shows it reads input; the enable of bracketed paste does not count. The wait uses the host's full `input.initial_startup_grace_ms` even when the request omits `extended_input_ready_wait`; a request that sets it waits up to the runtime's longer `[input] startup_grace_ms` when one is declared (the Hermes package declares 30000 ms). The grace is an upper bound: the input is written as soon as readiness shows. If no readiness is observed by the deadline, or the daemon begins shutting down first, nothing is written to the PTY, the call fails with `session_input_not_ready` (runtime class; a deadline names the grace and the session's last observed activity, such as blocked, and a shutdown names the shutdown instead), and the session is removed as for any failed initial-input create (a removal that cannot finish is completed by reconciliation). A runtime whose existence check declares how its agent moves its store refuses a launch that would move it out of reach with `agent_native_store_relocated` before anything is spawned, and with `native_store_check_task_panicked` if that check crashes (see "Native session references"). When the host lists `post-create` in `service.toml` `[hooks] required`, a worktree create whose `post-create` hook fails (a script that exits non-zero, outlives the hook timeout, cannot be spawned or waited on) fails with `hook_required_failed` (runtime class) instead of a `hook` warning. The first failing script ends the hook, so a repository script after a failing host script does not run; the `msg` names the event, the layer (`host` or `repository`), the script and how it ended (exit status, timeout), the scripts that did not run because of it, and the hook's stderr tail as the `hook` warning carries it. When the create had already reset a diverged local branch (`branch_source: remote`), the `msg` also names the backup ref that keeps its previous head. The create is then rolled back like any failed create: no PTY starts, no initial input is injected, `session-start` does not fire, the `pre-remove` and `post-remove` hooks run, and no session record, binding or worktree directory remains; the created branch is kept. A reused worktree runs no `post-create`, so `required` does not apply to it. A required hook that outlasts the client's request timeout ends the call with the outcome-unknown error while the daemon rolls the create back: inspect `session list` before retrying. A client of any protocol version in the window receives the code as an ordinary error code; a previous release's daemon ignores `required` and keeps the warning behaviour. Optional `secret_env` (a map of variable name to string value) fills the secret slots of the launched host profile: the profile's `secret_env = ["NAME", ...]` key declares the names a caller may fill and every declared name is required, so the names come from the host profile and only the values from the request (see "Secret session environment"). A request naming a variable the resolved profile does not declare fails with `secret_env_not_declared`, a launch that leaves a declared name unfilled with `secret_env_missing`, and a malformed map with `secret_env_invalid`, all before anything is created; none of the three quotes a name or a value. Clients check `HostCapabilities.session_new_secret_env_supported` first, because a daemon without it drops the field silently and would start the agent without its credential (the Rust client raises `session_new_secret_env_unsupported` without sending the request; protocol 3 peers refuse the field). |
| `session.list` | `SessionListParams` or `null` | `Vec<SessionInfo>` | Lists sessions; filters use AND semantics. Metadata filters require the current protocol; a client refuses them against a protocol-3 daemon with `daemon/daemon_protocol_too_old` before sending the request. |
| `session.inspect` | `SessionId` | `SessionInfo` | `SessionId` is a JSON string, e.g. `"s-1"`. |
| `session.stop` | `SessionId` | `SessionStopResult` | Stops a live session (the entry stays in `list`); it ends `stopped` with its root's exit. When a process outside the session's process group keeps the PTY open past the stop deadline, the worker force-closes the PTY output and the stop still succeeds with `SessionInfo.output_force_closed` set, because output written after the close is not retained. Before it records that exit, the worker sweeps every same-user process carrying the runtime's `POHUNEK_WORKER_INSTANCE_ID` marker (`SIGTERM`, then `SIGKILL` after the stop grace) and, while only processes whose environment cannot be read remain, re-reads the process table for up to the stop grace; if it still cannot prove every such process gone, it faults the runtime instead, the stop fails with `worker_operation_failed`, nothing is signalled that it could not attribute, and the session's runtime becomes `lost` (`runtime_lost_cleanup_unconfirmed` when the daemon's own sweep cannot confirm either). A stop whose root exit cannot be observed fails. A `conflict` session is stopped by the identity its record names, only after the recorded generation's journal names the recorded worker and the service manager's job under that generation is that worker's: `session_runtime_conflict` (no recorded generation or worker), `runtime_identity_mismatch` (no journal names the recorded worker, or the job is not that worker's), `runtime_supervision_ambiguous` (journals unreadable, no job holds the still-running worker process, or the ownership-marker sweep cannot confirm every marked process exited), `runtime_supervision_unavailable` (job cannot be inspected). A refusal leaves the record unchanged. A session that already ended, which includes a lost runtime whose end was proven (it presents as `failed`), returns `{stopped: false}`. A `lost` runtime whose end was not proven, a `reconnecting` one and an `incompatible` one are refused with `session_runtime_lost`, `session_runtime_reconnecting`, and `worker_protocol_incompatible`. The call is not cancelled when the client hangs up. The Rust and TypeScript clients wait up to 90 minutes on a dedicated connection and report an elapsed deadline as `session_stop_outcome_unknown`; a repeated stop of a stopped session returns `{stopped: false}`. The daemon bounds its wait for the worker's stop answer and ends a frozen worker's stop with `worker_operation_failed`. |
| `session.resume` | `SessionResumeParams` (a bare `SessionId`, or `{session_id, accept_profile_change?}`) | `SessionResumeResult` | Explicitly recovers a terminal or lost logical session from captured native recovery metadata, reusing the logical session id but starting a new worker generation (a new native service job) with a new runtime id. The previous generation must be proven ended first; two generations of one session are never live at once. A lost runtime with `runtime_lost_cleanup_unconfirmed` requires a fresh, confirmed sweep of the exact runtime its journal names before a new generation is minted. When proving the previous runtime ended, missing or mismatched supervision evidence, or a sweep that still cannot prove the marked processes gone, returns `runtime_supervision_ambiguous` with a recovery hint and leaves the logical session recoverable for a later retry. When same-user processes whose environment cannot be read are the only obstacle, the error `msg` lists them (`pid`, start identity, command name; at most eight, then `and N more`; command names escaped as for `session.remove`) and no environment values are included. The call is not cancelled when the client hangs up, so a client that stopped waiting cannot tell whether the resume still completes: inspect the session before retrying. Before any recovery write or worker launch, recovery resolves the frozen program again and fails with `agent_binary_missing` when it no longer resolves to an executable; a runtime that declares a version probe (Hermes included) then revalidates that executable and returns `agent_runtime_unsupported`, with the same message as `session.new`, when the probe rejects it. Live, reconnecting, conflicting, or incompatible runtimes are rejected; a hook runtime without a native claim returns `native_identity_missing`, and one with a claim that was not verified as its launch process returns `native_identity_unverified`; an unreadable durable record, or absent, unreadable, ambiguous, or worker-id/instance-mismatched native generation evidence returns `native_identity_evidence_unavailable` before launch; a hook runtime whose tested worker journal carries a newer conversation switch that this daemon cannot verify refuses recovery with `native_identity_uncertain` instead of silently resuming the older verified target; other sessions without native metadata return `not_resumable` or `agent_not_resumable`. A Claude conversation id whose transcript cannot be found or verified under the effective Claude config home returns `agent_native_reference_missing` before a worker launches. A session launched from a host profile relaunches only while the profile still has the revision frozen into the session (see "Frozen profile revision" below): an edited profile, or a legacy session that never froze one, returns `agent_profile_changed`; a profile that no longer resolves returns `agent_profile_missing`; a host that cannot read its revision key returns `agent_profile_revision_unavailable`. An adopted built-in session whose profile differs only by the migration pin (`package` and `digest` keys) resumes and forks without the decision. `accept_profile_change: true` is the owner's decision to relaunch under the current profile and freeze its revision; it is accepted on the local control socket only and a remote caller gets `agent_profile_change_local_only` before anything is resolved. The bare id shape stays valid on every protocol version of the window and never accepts a change. Before anything launches, the declared existence check refuses with `agent_native_store_relocated` when the agent's store moved out of reach since the launch, and with `agent_native_reference_missing` when the conversation is not found (`native_store_check_task_panicked` when the check itself crashes). Daemon restart never calls this method automatically. |
| `session.fork` | `SessionForkParams` | `SessionForkResult` | Forks a native agent conversation into a new pohunek session id and PTY, using the source session's cwd/worktree for `cwd_mode: "same"`. Live sources are allowed. Unknown ids return `session_not_found`; external sessions return `session_external_read_only`; sources without launch-agent native metadata return `not_resumable` or `agent_not_resumable`; a source whose native generation evidence cannot be read or matched refuses before launch with `native_identity_evidence_unavailable`, and a newer unverified conversation switch refuses with `native_identity_uncertain`; an assigned reference whose conversation cannot be found returns `agent_native_reference_missing` before anything launches; a runtime that declares `assign_fork` launches the fork under a reference generated for it, returned as the fork's `native_session_id` and resumable at once; Codex- and Hermes-backed sessions return `agent_fork_unsupported`; unmigrated legacy resume bindings return `migration_manifest_missing` as for `session.new`. The fork follows the same frozen-profile rules as `session.resume` (`agent_profile_changed`, `agent_profile_missing`, `agent_profile_revision_unavailable`, local-only `accept_profile_change` in `SessionForkParams`) and freezes the revision it launched under. The program and version-probe checks of `session.resume` apply before anything launches (`agent_binary_missing`, `agent_runtime_unsupported`). The source's existence check runs first, with the same `agent_native_store_relocated`, `agent_native_reference_missing` and `native_store_check_task_panicked` refusals as `session.resume`, and a refused fork creates no child. A successful fork emits `session_created`. The call is not cancelled when the client hangs up; an elapsed client deadline is `session_fork_outcome_unknown` and a retried fork creates a second child, so list sessions first. |
| `session.remove` | `SessionId` | `SessionRemoveResult` | Evicts a session from the registry, stopping it first if still live. Unknown id is `session_not_found`. A `reconnecting` or `incompatible` runtime, or a `conflict` whose reason is `runtime_supervision_ambiguous`, is retired through the service manager by the exact worker generation its record names, and every worker the session's journals record for that generation must then be gone, before the record is deleted. A record in one of those states that names no generation, or a `conflict` for any other reason, fails with `session_runtime_conflict`, `session_runtime_reconnecting`, or `worker_protocol_incompatible`; a retirement the service manager cannot complete fails with `runtime_supervision_unavailable`; a still-running worker journaled under another generation fails with `runtime_identity_mismatch`; unreadable session journals or a journaled worker still running after the retirement fail with `runtime_supervision_ambiguous`. The record is kept in every refused case. Every removal then sweeps the processes carrying the session's runtime ownership markers (every runtime its record or a journal of its generation names); a runtime named only by the record is signalled only when its process also carries this session's marker. A missing session marker leaves removal retryable even with `session.remove_accepting_unconfirmed`. A sweep that cannot confirm every marked process exited fails with `runtime_supervision_ambiguous` after the removal intent was recorded, keeping the session listed so a retried removal, or the next daemon start, finishes it. When same-user processes whose environment cannot be read are the only obstacle, the error `msg` lists them (`pid`, start identity, command name; at most eight, then `and N more`) and `recover` tells the operator to inspect and end the ones belonging to the session and retry; no environment values are included. It always refuses on an unconfirmed sweep; `session.remove_accepting_unconfirmed` is the consent variant. Worktree cleanup is best-effort: the result reports `worktrees_removed` for checkouts that are gone and `worktrees_failed` for an owned checkout whose `git worktree remove` failed, whose binding is dropped regardless, so the leftover directory needs manual cleanup. A failing or timed-out worktree `pre-remove` or `post-remove` hook never blocks the removal; the result's optional `warnings` (omitted when empty) carries a `hook` warning that names the event, because the session record is gone once the call returns. This is an explicit operator removal and is deliberately not subject to the retention sweep's unsaved-work hold. The call is not cancelled when the client hangs up; an elapsed client deadline is `session_remove_outcome_unknown`. A worker that stops answering the removal's stop ends it with `worker_operation_failed` and keeps the record. |
| `session.remove_accepting_unconfirmed` | `SessionId` | `SessionRemoveResult` | Removes a session exactly like `session.remove`, except that when the marker sweep of its runtimes is unconfirmed solely because same-user processes with unreadable environments may belong to them, it proceeds instead of refusing with `runtime_supervision_ambiguous`. The daemon never signals those processes; it logs each accepted process at `warn` once the sweep lets the removal proceed, before any cleanup, and lists the set in `accepted_unconfirmed_processes` (`pid`, `start_identity`, optional `command`; the field is omitted when empty). The list is deduplicated and holds the first 64 accepted processes; when more were accepted, `accepted_unconfirmed_total` carries the full count (the field is omitted otherwise, and a reader that does not know it ignores it) and the daemon log names every one. A removal with more than 1024 candidates is refused with a count of at least the candidates found before anything is deleted, because every accepted process is logged. Each `command` is a kernel command name with every character outside ASCII letters, digits, space and `._-+:@/` escaped as `\u{..}` and long names shortened with `...`, so it is safe to print. Consent covers this one call only: nothing stores it, a retried removal needs it again, and the reconciliation finalizer and the retention sweep never have it. Every other unconfirmed reason (a signalled process still running, a sweep error, a missing supervision configuration) and every other refusal of `session.remove` still applies with the same error codes. A process among the accepted ones that does carry the runtime marker keeps running unsupervised after the worktree, logs and record are deleted. A daemon without this method answers `method_not_found`. Its client deadline and outcome-unknown error are those of `session.remove`. |
| `session.runtime_inventory` | `null` | `RuntimeInventoryResult` | Returns the durable-worker runtime inventory captured at startup reconciliation: one `RuntimeInventoryEntry` per discovered worker with its runtime slot, claimed session id, worker/runtime ids, and classification (`managed`, `orphaned`, `conflict`, `incompatible`, or `identity_mismatch`). Read-only operator diagnostic; it never mutates or kills a worker. A worker `status` a newer daemon defines reads as `unknown` on the Rust client and the CLI. |
| `session.attach` | `SessionAttachParams` | `SessionAttachResult` | Mints a one-shot attach stream id. |
| `session.detach` | `SessionDetachParams` | `SessionDetachResult` | Cancels an active attach stream. After a worker stream failure, the first call returns its optional typed `error` and consumes that short-lived result; unknown or already-consumed streams return `detached: false` without `error`. |
| `session.resize` | `SessionResizeParams` | `SessionResizeResult` | Resizes the PTY on the control connection. |
| `session.input` | `SessionInputParams` | `SessionInputResult` | Injects text using agent-specific input framing. Hermes accepts at most `MAX_SESSION_INPUT_BYTES` UTF-8 bytes, permits LF and tab but rejects other C0/C1 controls without rewriting them. Hermes (through `text_policy = "hermes_safe_text"`) and a runtime whose descriptor declares `[input] refuse_while_blocked = true` (the Pi package, whose dialogs replace its editor) return `session_input_blocked` for fire-and-forget input while the activity is `blocked`; input with `wait` is refused for every runtime with `session_agent_blocked`; the error carries a `recover` hint that names `session.attach` (`pohunek attach`), the interactive way to answer the dialog. The refusal is absolute for `session.input`: there is no caller option that targets a dialog. Unsafe or oversized Hermes text returns `session_input_rejected`. Every per-session input holds one gate through its complete framing transaction, preventing waited/waited and waited/fire-and-forget interleaving. Zero-delay input sends one worker plan with a body fragment and a separate submit fragment; delayed-framing input, waited or fire-and-forget, sends two single-fragment plans (body, then submit) with the daemon-side submit delay between them. Body and Enter are never merged into one paste burst. With `wait`, every Rust/TypeScript typed call path, including generic `Client.call`, routes through the dedicated helper, normalizes absent `until` to `[]` before wire validation, and rejects `timeout_ms` outside `1..=8000` before transport. The daemon deduplicates targets, acquires a waiter permit, starts the overall deadline before the input gate, and rejects blocked activity with `session_agent_blocked` both before the gate and at the causal boundary, independently of provider fire-and-forget policy. Waited input on nonzero-delay framing is daemon-paced: the daemon writes the body as one worker plan, waits out the submit delay, re-checks activity and the runtime identity under the session lock, and only then writes Enter as a second plan, capturing the runtime-scoped activity revision boundary immediately before Enter's send starts. The session input gate stays held across both writes, so no other input to the session interleaves. Ordering guarantee: the submit is written only after the body was flushed and acknowledged and at least the submit delay later, and no byte of another writer lands inside a fragment. The worker control connection is released once the body is acknowledged and is not held during the delay, so other control requests such as resize, stop and observation are served while the delay runs, and attach input may land between the two plans. Fire-and-forget input succeeds when the agent's process ended after the text was delivered, because Enter then has nothing to reach. A control plan carrying its own `delay_after_ms` between fragments keeps the same rule: other control plans never interleave with it, and the worker's PTY lock is released while the delay runs. If the agent turned `blocked` during the delay, or the deadline or a daemon shutdown arrives before Enter is claimed, Enter is not written and the call fails with `session_input_not_submitted`; fire-and-forget input to a runtime that refuses input while blocked (Hermes, Pi) re-checks only the activity under the session's input rules before it writes Enter (the typed text may remain in the agent's input box and cannot be retracted); a replaced or exited runtime fails with `session_runtime_changed` or `session_not_running` and never receives Enter. A deadline or shutdown that arrives before the body is written, and a deadline no longer than the submit delay, write nothing and fail with `session_input_timeout` or `daemon_shutting_down`. On nonzero-delay framing an `idle` target (explicit, or through the default `idle` and `blocked` targets) is satisfied only by `idle` evidence that follows `working` evidence above the boundary, because the prompt box reads as `idle` until Enter is written; use `--until working` to confirm that the agent started on the input. Zero-delay framing acquires the exclusive worker write reservation before capturing runtime-scoped activity revision evidence immediately before send. Timeout or shutdown before send cancels the reservation and writes no bytes. After send begins, the exchange is cancellation-shielded and retains the session input gate until the late ACK is consumed. Matching evidence above that lower bound and observed through the fixed deadline succeeds, including evidence between PTY plan flush and worker ACK. After the plan is sent delivery outcome may be unknown, so callers inspect the session and do not retry blindly. Bounded evidence history retains the maximum wait window, so a later same-activity report cannot erase valid pre-deadline evidence. The result includes `activity`, `activity_source`, `runtime`, `activity_epoch`, decimal-string `activity_revision`, and `submitted: true` (Enter was written and matching evidence followed; absent for fire-and-forget input and from daemons that predate the field, so SDK helpers do not require it); SDK helpers require all five and return `session_input_wait_contract_mismatch` when a same-version daemon ignores `wait` or omits evidence, except for the prompt-only success below. `wait.allow_prompt_accepted: true` (the CLI sets it for every `--until` or `--timeout` wait) lets the Claude `UserPromptSubmit` hook report answer a `working` target when no `working` activity was observed: the result then has `prompt_accepted: true`, `submitted: true`, `runtime` and `activity_epoch`, and no `activity`, `activity_source` or `activity_revision`, because no activity was observed; SDK helpers accept that shape only from a caller that opted in and asked for `working`. An opted-in result of a Claude session on worker protocol 8 or newer whose hook schema admits the prompt action carries `prompt_accepted` (`false` when only activity answered); the field is absent otherwise (other runtimes, an older worker), where the timeout keeps the generic wording; a daemon that predates the field ignores the option and answers on activity alone, and a protocol 3 connection drops it. A report counts only when the hook started after the worker wrote the plan that carries Enter, for the submission's own runtime, before the deadline, and when the agent was not `working` at the boundary (a busy agent queues the input and its own `working` evidence answers). It never answers an `idle` target, nor the `working` evidence that delayed-submit framing requires before `idle`. A turn the agent starts on its own between the Enter write and the agent reading Enter makes the wait succeed on that report, so an acceptance means the prompt was submitted or queued. When neither activity nor an acceptance follows Enter, the wait ends with `session_input_timeout`, whose message names both for an opted-in `working` wait. Clients deduplicate by `(activity_epoch, runtime, activity_revision)`. Runtime exit returns `session_not_running`; a lost runtime returns `session_runtime_lost` (also when its proven loss made the session `failed`); replacement returns `session_runtime_changed`; external sessions return `session_external_read_only`; shutdown cancels delivery or waiting. A write the agent does not read for the configured stall bound (`[input] write_stall_timeout_ms` in `service.toml`, 3000 ms by default; the bound restarts after every byte the PTY accepts) fails with `session_input_stalled`, and the write may have delivered part of its plan, so the caller inspects the session before resending. The dedicated connection adds fixed response headroom beyond the overall deadline. |
| `session.screen` | `SessionScreenParams` | `SessionScreenResult` | Reads one bounded, rendered, runtime-bound terminal snapshot without acquiring attach ownership or resizing the terminal. |
| `session.detection` | `SessionDetectionParams` | `SessionDetectionResult` | Read-only detector diagnostic. Returns the complete supported region-kind set and current text previews for regions required by the session's active manifest. A region a newer daemon defines is dropped from `supported_regions` on the Rust client and the CLI, and a preview of such a region shows `kind` `unknown`. |
| `session.read` | `SessionReadParams` | `SessionReadResult` | Reads bounded text from the current rendered terminal without attaching. `lines` accepts `1..=1,000`, defaults to that ceiling, and keeps the newest rendered rows. The complete serialized result is capped by `MAX_SESSION_READ_RESPONSE_BYTES`; byte truncation keeps the newest UTF-8 text and sets `truncated`. ANSI is a reserved request value that v2 always rejects. A `source_used` a newer daemon defines reads as `unknown` on the Rust client and the CLI; `source` in params stays strict, so `unknown` is `bad_request`. |
| `session.output` | `SessionOutputParams` | `SessionOutputResult` | Reads a newest retained tail or continues from an exact runtime-scoped output cursor. A request with `wait_ms` uses a dedicated connection with bounded-wait headroom, and is answered with `daemon_shutting_down` when daemon shutdown starts during the wait. |
| `session.wait` | `SessionWaitParams` | `SessionWaitResult` | Performs one bounded long poll for state, activity, metadata, terminal, output, or runtime change. It always uses a dedicated connection, and it is answered with `daemon_shutting_down` when daemon shutdown starts during the wait. A `reason` a newer daemon defines reads as `unknown` on the Rust client; `pohunek session wait` exits non-zero with `session_wait_reason_unknown` (under `--json` the error document is the only output), because it cannot say whether the condition matched. |
| `session.wait_turn` | `SessionWaitTurnParams` | `SessionWaitTurnResult` | Waits for the next turn event (`turn_completed`, `agent_blocked`, `approval_required`, `exited`) of one or more sessions after an opaque cursor, bounded by an agent-chosen timeout under a daemon ceiling; a timeout returns per-session progress. It always uses a dedicated connection. New in protocol 4: a protocol 3 client or daemon does not define it. See [Turn Waits](#turn-waits). A `reason` a newer daemon defines reads as `unknown` on the Rust client and the CLI; the result keeps its cursor and progress, so chained waits continue. |
| `session.report_agent` | `SessionReportAgentParams` | `SessionReportAgentResult` | Hook callback for nested agents running inside an existing session. It records an active-agent claim, optional process binding (`pid`, confirmed by an optional `pid_start_identity`), and optional active native metadata without changing launch identity or resume binding; ignored reports return `recorded: false`. Claims are reconciled with process facts and can be auto-released when no live backing process remains. Accepts the optional `retryable_admission` flag described under Retryable admission. |
| `session.release_agent` | `SessionReleaseAgentParams` | `SessionReleaseAgentResult` | Hook callback that clears a matching active nested-agent report and restores the session's default detector identity. Claude `SessionEnd` hooks use this as the clean-exit fast path; non-current releases return `released: false`; process-backed auto-release uses the same clear path. Accepts the optional `retryable_admission` flag described under Retryable admission. |
| `session.report_native_id` | `SessionReportNativeIdParams` | `SessionReportNativeIdResult` | Hardened public fallback for launch-agent resume metadata. Reports are runtime- and process-bound, ordered, expiring, and provider-matched; ignored reports return `recorded: false`. The caller is attested with its kernel peer credentials before any session state is read: only the session's PTY root itself is admitted, and a caller that fails the attestation (a remote connection, a descendant of the root, another user, or a root whose session uses the `identity-v1` schema) gets the error `native_report_unattested` (see the native-id report attestation rules below the schema section). The owner-private worker claim path is preferred. This is not the nested-agent active identity callback. Accepts the optional `retryable_admission` flag described under Retryable admission. |
| `session.set_metadata` | `SessionSetMetadataParams` | `SessionSetMetadataResult` | Merges owner-controlled metadata. Values must not contain secrets. The edit is written to the session's durable record before the session shows it, so it survives a daemon restart; a refused write fails with `session_store_failed` and changes nothing. |
| `session.rename` | `SessionRenameParams` | `SessionRenameResult` | Sets or clears a session's owner display name (`name: null` clears). Cosmetic; the daemon trims it and rejects a control character or over-long name. |
| `session.diff` | `SessionDiffParams` | `SessionDiffResult` | Computes a unified diff of a session's worktree against a base ref. `base: null` defers to the worktree binding's recorded base branch, then the repository default. A session without a bound worktree returns `session_no_worktree`; a hostile explicit `base` (empty, leading `-`, or a control character) returns `invalid_branch`; a `base` that cannot be resolved to a merge-base against `HEAD` returns `session_diff_base_unresolved`. Incomplete git output (a pipe still open 250 ms after git exits, or more than 8 MiB on either captured stream) returns `session_diff_failed` rather than a partial successful diff. See `SessionDiffResult` under Core Payloads for the response size cap and truncation semantics. |
| `session.policy.get` | `null` | `SessionPolicyResult` | Reads the daemon's session retention policy. Non-null params are `daemon/bad_request`. |
| `session.policy.set` | `SessionPolicyParams` | `SessionPolicyResult` | Validates, replaces, and persists the session retention policy at `<data_dir>/session-policy.json`. The running sweep task reloads it on its next cycle, so no daemon restart is needed. |
| `session.idle_stop.get` | `null` | `SessionIdleStopResult` | Reads the host default of the idle-stop duration (`default_after_secs`, absent when none is set) and the smallest duration the host accepts (`min_after_secs`). Non-null params are `daemon/bad_request`. New in the current protocol window: a protocol 3 client or daemon does not define it, and a daemon of the previous release answers `method_not_found`. `host.inspect` advertises it as `session_idle_stop_supported`. See [Idle stop](#idle-stop). |
| `session.idle_stop.set` | `SessionIdleStopParams` | `SessionIdleStopResult` | Validates, replaces and persists the idle-stop default at `<data_dir>/session-idle-stop.json`. An omitted or `null` `default_after_secs`, and `0`, clear it; a value below `min_after_secs` is `runtime/invalid_session_policy`. It never reads or writes the retention policy, so `session.policy.get` and `session.policy.set` keep their shape. |
| `session.retention.sweep` | `SessionRetentionParams` or `null` | `SessionRetentionResult` | Runs one retention sweep under the current policy TTLs, or reports the selection when `dry_run` is true. It works whether or not automatic sweeps are enabled, never exceeds the policy's own per-sweep removal cap, and never selects an `external` session or one in `conflict`/`incompatible` runtime state. A matched session whose pohunek-owned worktree has an uncommitted change, an untracked file, commits contained in no other branch/remote/tag, or a state git cannot report is **held**, never removed: it is counted in `held`, excluded from `eligible`, and carries the `hold` reason (`worktree_uncommitted`, `worktree_untracked`, `worktree_unpushed`, `worktree_unknown`) on its candidate. One sweep inspects at most 64 checkouts; a matched session beyond that bound is held as `worktree_unknown` and inspected by the next sweep. `worktrees_cleaned` counts only checkouts confirmed gone from disk; a checkout that survived its removal is reported in `worktrees_failed`. The `pohunek session retention sweep` CLI exits non-zero when `failed` or `worktrees_failed` is above zero. A `reason` or `hold` a newer daemon defines reads as `unknown` on the Rust client and the CLI; an unknown `hold` stays a hold and is never read as `None` (free to remove). |
| `subscribe` | `null` | `{subscribed: true}` then event stream | Consumes the connection into a one-way event stream. |
| `integration.install` | `IntegrationInstallParams` or `null` | `IntegrationInstallResult` | Installs agent hooks for active-agent state, native session id capture, provider-managed subagent lifecycle, and notifications into the config home of the selected runtime(s). Each report carries `cleanup_incomplete`, the quarantine paths of replaced originals whose deletion did not finish after the install committed (empty when cleanup completed). `profile` selects the config home a host profile launches with and `all_profiles` every distinct home of the runtime (see "Config homes and host profiles"); both are local-only, exclusive, and unknown fields return `bad_request`. With `all_profiles` a home that fails is listed in `failed` (`{agent, home, error}`) while the others still install, and the result is not atomic across homes. Without `all_profiles` every selected home is resolved before the first write, so a home that does not resolve refuses the request with nothing written. Without `agent` on a host where no served runtime has daemon-managed hooks (a clean host: Hermes is CLI-run and the shell has no handler) the request fails with `runtime/integration_no_managed_runtime`, whose hint names the install of each official package with daemon-managed hooks (Codex, Claude Code) that nothing serves and then `pohunek integration install --agent <runtime>`; the Hermes operator plugin is installed by the CLI with its own target flags and is never named. A runtime whose handler installs nothing persistent (Muse) reports an install with an empty `hook_path` and no config paths; its hooks are set up for each launch. |
| `integration.status` | `IntegrationStatusParams` or `null` | `IntegrationStatusResult` | Returns a read-only per-agent report for managed Codex and Claude hooks: availability, expected and present asset paths, inspected registration paths, installed and expected versions, aggregate health (`not_installed`, `current`, or `outdated`), typed recovery (`none`, `reinstall`, or `repair_configuration`), and non-secret warnings. On the Rust client and the CLI an install state or recovery a newer daemon defines reads as `unknown`: never `current`, and the recovery hint is generic (upgrade the client). `null` selects every runtime the daemon serves through a daemon-run hook handler; unknown parameter fields return `bad_request`. `profile` and `all_profiles` select profile config homes (local-only; see "Config homes and host profiles") and label each report with `home`. `current` requires exact managed executable permissions and exactly one registration under each installer-owned event. Inspection is size-bounded and runs outside the Tokio request task. It never mutates provider configuration. For a runtime whose handler installs nothing persistent (Muse), `current` means the config home's settings and the managed hooks file they select build a valid overlay; a managed hooks file named only by a profile `[env]` or the service environment is checked at launch, not here. |
| `integration.uninstall` | `IntegrationUninstallParams` (`agent` required) | `IntegrationUninstallResult` | Removes exactly the assets and registrations the installer owns for Codex and Claude, as one rollback-protected transaction under the installer lock: registration first, then Codex trust records, then marker-carrying scripts. User hooks, the Codex hooks feature flag, the hooks directory, and the lock file are left alone; a symlink, directory, FIFO, or unmarked file at a managed script path is preserved and reported in `preserved_paths`. Each report carries `state` (`removed` or `not_installed`), `removed_paths`, `updated_paths`, `preserved_paths`, and `cleanup_incomplete` (quarantined originals whose deletion did not finish after the removal committed). `agent` names the one agent to remove and is required: a missing agent, unknown fields, and `null` params return `bad_request`, so removal never widens to every agent and a failure cannot leave one agent removed without a report. `profile` selects one profile's config home and `all_profiles` every distinct home of that agent, one transaction per home with failures listed in `failed` and no atomicity across homes (local-only; see "Config homes and host profiles"); an absent `agent` is still `bad_request` with either. Ownership is judged on the inode that is deleted, and a provider file whose content, mode, or inode changed since it was read is a collision. The same re-verification runs before any rollback, so a file another writer changed is kept there too rather than replaced by the original. Every rollback or cleanup outcome that leaves data in quarantine is reported with its true path, as `integration_recovery_required` during a rollback and as `cleanup_incomplete` after commit. A registration file the removal wrote that another writer changed before the scripts are removed, or before the removal completes, is also a collision: it is re-verified (inode, mode, complete content), the other version is kept, and the removed scripts are moved back. Collisions, interrupted rollbacks, and concurrent installers return `integration_destination_collision`, `integration_recovery_required`, and `integration_install_in_progress`. |
| `integration.doctor` | `IntegrationDoctorParams` or `null` | `IntegrationDoctorResult` | Read-only diagnosis of the managed Codex and Claude hooks: per agent `ok`, the status report (absent while an operation is running), and findings with a stable snake_case `code`, `severity` (`info` or `error`), `summary`, and `remediation`. A code or severity a newer daemon defines reads as `unknown` on the Rust client and the CLI, which render it as such and re-emit `"unknown"` under `--json`; the exit status stays driven by `ok`. Only the absence expected of a not-installed integration is informational: an unsafe, symlinked, or group-writable config path is an error even when nothing is installed. Codes cover absent agents and hooks, config-root, asset, registration, provider-config, and Codex feature/trust drift, `displaced_original_left_behind` (a quarantined original from an earlier install or removal remains in the agent config directory) and `quarantine_scan_incomplete` (the bounded scan could not cover every entry, so it is an error and the diagnosis is never clean), and `operation_in_progress` (info: another install, uninstall, or doctor holds the installer lock, so nothing is read or scanned for that agent, `status` is absent, and the doctor stays `ok`; the doctor takes the lock non-blocking without creating the file and holds it for its whole inspection, so an installer arriving meanwhile gets `integration_install_in_progress`), `unsafe_installer_lock` (error: the lock file is not a regular owner-private file, so every install and uninstall fails), `install_drift`, and three informational `python3` notes that never fail the doctor or change its exit code, because the daemon cannot know the agent's `PATH`: `hook_runtime_python_found` (the first executable `python3` behind an absolute entry of the daemon's own `PATH`), `hook_runtime_missing` (none found; relative and empty entries are skipped), and `hook_runtime_macos_shim` (the first one is the `/usr/bin/python3` stub, recognized through any alias, with no Command Line Tools or Xcode behind it); the probe never runs an interpreter and the remediation is to verify `python3` on the agent's `PATH`, and `hook_socket_path_invalid` (daemon or worker socket path over the platform limit). The runtime notes apply only to an agent whose hooks are installed. `null` selects every runtime the daemon serves through a daemon-run hook handler; unknown fields return `bad_request`. `profile` and `all_profiles` diagnose profile config homes (local-only; see "Config homes and host profiles"); each diagnosis carries `home`, and the recovery commands in its findings name `--profile` for a profile's home. |
| `assistant.materialize` | `AssistantMaterializeParams` | `AssistantMaterializeResult` | Materializes the assistant knowledge bundle on the daemon host. A concept `type` a newer daemon defines reads as `unknown` on the Rust client, so a mismatching bundle ends in the typed `assistant_bundle_mismatch` error rather than a decode error; an `intents` element it defines is dropped. |
| `notification.create` | `NotificationCreateParams` | `NotificationCreateResult` | Creates a host-local notification. Daemon policy is enforced for every producer, including provider hooks and daemon projectors. Dedupe may return `created: false` with an existing or upgraded record. `agent_blocked`/`approval_required` with `attention:<session_id>` and `turn_completed` with `turn:<session_id>` are deferred: the result still reports `created: true` with a minted id, but the record is held pending until `attention_debounce_secs` elapses; see `NotificationPolicy`. Accepts the optional `retryable_admission` flag described under Retryable admission. |
| `notification.list` | `NotificationListParams` or `null` | `NotificationListResult` | Lists notification records with exact-match filters and cursor pagination. Deleted records are excluded unless `status: deleted` is requested. |
| `notification.update` | `NotificationUpdateParams` | `NotificationUpdateResult` | Updates one record's lifecycle status. Allowed transitions are `unread -> read`, `read -> acknowledged`, `unread -> acknowledged`, `unread/read/acknowledged -> archived`, and any non-deleted status to deleted. |
| `notification.delete` | `NotificationDeleteParams` | `NotificationDeleteResult` | Logically deletes one record. Unknown or already-deleted ids return `deleted: false`. |
| `notification.policy.get` | `null` | `NotificationPolicyResult` | Reads the daemon's notification policy. Non-null params are `daemon/bad_request`. |
| `notification.policy.set` | `NotificationPolicyParams` | `NotificationPolicyResult` | Validates, replaces, and persists the daemon notification and automatic-retention policy at `<data_dir>/notifications/policy.json`. |
| `notification.retention.prune` | `NotificationRetentionParams` or `null` | `NotificationRetentionResult` | Explicitly deletes records selected by retention filters, or reports matches when `dry_run` is true. Applied pruning may also compact the action log at the policy threshold. |
| `project.list` | `ProjectListParams` or `null` | `Vec<ProjectInfo>` | Lists known projects on the target host. A `source` a newer daemon defines reads as `unknown` on the Rust client and the CLI; `--filter source=` still accepts only `auto` and `manual`. |
| `project.add` | `ProjectAddParams` | `ProjectInfo` | Registers a host-local git project path. |
| `project.show` | `ProjectShowParams` | `ProjectShowResult` | Shows a project plus live worktree state. Each `ProjectWorktree` reports `owned` when pohunek created it; `owner_session_id` names the session it was created for and stays after that session stops; `session_id` names a live (non-terminal) session running in it. Each of the two ids is omitted, never `null`, when it does not apply: `owner_session_id` on a worktree pohunek does not own, `session_id` when no live session runs there. Clients older than `owner_session_id` ignore it. |
| `project.rename` | `ProjectRenameParams` | `ProjectInfo` | Sets a custom display label. |
| `project.remove` | `ProjectRemoveParams` | `ProjectRemoveResult` | Removes a project record and optionally owned worktrees. |
| `project.prompt` | `ProjectPromptParams` | `ProjectPromptResult` | Resolves a prompt template without rendering it. A `layer` a newer daemon defines reads as `unknown` on the Rust client and the CLI. |
| `project.action` | `ProjectActionParams` | `ProjectActionResult` | Resolves one action recipe plus prompt content. A `provider` a newer daemon defines reads as `unknown` on the Rust client and the CLI, never as `none`, which stays the provider-less kind. |
| `project.actions` | `ProjectActionsParams` | `ProjectActionsResult` | Lists available project actions after layer shadowing. |
| `worktree.remove` | `WorktreeRemoveParams` | `WorktreeRemoveResult` | Removes one owned worktree binding, refusing live sessions unless forced by the method contract. |
| `package.list` | `null` | `PackageListResult` | Local-only. Lists installed runtime packages in ascending digest order with the registry `generation` and per-package health. See [Runtime Package Methods](#runtime-package-methods). An `origin`, `fault` or `selection_blocked.reason` a newer daemon defines reads as `unknown` on the Rust client and the CLI; an unknown origin keeps the official-downgrade guard of `plugin update` engaged, and an unknown block reason keeps the package marked as selection blocked. |
| `package.inspect` | `PackageInspectParams` | `PackageInspectResult` | Local-only. Returns one installed package record and what its runtime descriptor declares. |
| `package.doctor` | `PackageDoctorParams` or `null` | `PackageDoctorResult` | Local-only. Verifies every installed package root and reports faults, unregistered roots and pinned digests that are not installed. A finding `kind` a newer daemon defines reads as `unknown` on the Rust client and the CLI; the finding is kept, so `plugin doctor` still exits non-zero. |
| `package.install` | `PackageInstallParams` | `PackageInstallResult` | Local-only. Installs a runtime package archive under explicit-digest or catalog trust, or previews it with `dry_run`. A `status` a newer daemon defines reads as `unknown` on the Rust client and the CLI. |
| `package.link` | `PackageLinkParams` | `PackageInstallResult` | Local-only. Copies a developer package directory into content-addressed storage and installs it disabled and unselected. |
| `package.set_enabled` | `PackageSetEnabledParams` | `PackageChangeResult` | Local-only. Enables or disables an installed package for fresh launches. |
| `package.select` | `PackageSelectParams` | `PackageChangeResult` | Local-only. Selects the installed version bare requests of its package id resolve to. |
| `package.uninstall` | `PackageUninstallParams` | `PackageUninstallResult` | Local-only. Removes an installed package that no session or host profile pins. |
| `package.bind_profile` | `PackageBindProfileParams` | `PackageBindProfileResult` | Local-only. Pins a host agent profile to an installed package under the package lifecycle authority, or previews the change with `dry_run`. A `status` a newer daemon defines reads as `unknown` on the Rust client and the CLI and is never treated as `unchanged`: `plugin profile migrate` still asks for consent. |
| `store.rejected_list` | `null` | `StoreRejectedListResult` | Local-only. Lists the metadata-store lines no loader accepts. See [Rejected Store Lines](#rejected-store-lines). |
| `store.rejected_set_aside` | `StoreRejectedSetAsideParams` | `StoreRejectedSetAsideResult` | Local-only. Moves the rejected lines with one content hash into the quarantine file. See [Rejected Store Lines](#rejected-store-lines). |
| `command.run` | `CommandRunParams` | `CommandRunResult` | Runs one command an installed package declared, once, and returns its stdout. Served on the local socket and, for a `peers` command, on remote overlay connections. See [Package Commands](#package-commands). |

`status` exists as a method constant in `crates/protocol` but is not a supported
daemon method in this API version. It returns `daemon/method_not_found`.

`HostCapabilities` advertises `terminal_read_supported`,
`output_read_supported`, `session_wait_supported`,
`session_wait_turn_supported`, `session_new_cwd_supported`,
`session_new_branch_source_supported`,
`session_new_input_ack_supported`,
`session_new_secret_env_supported`, `session_idle_stop_supported`,
`package_install_commands_supported`, and `package_bind_profile_expected_supported` independently. Clients must check the relevant
flag instead of assuming that a reachable daemon or an attach-capable session
supports every observation method. A daemon that predates `session.wait_turn`
omits `session_wait_turn_supported`, which decodes as `false`; it answers the
method itself with `daemon/method_not_found` when it speaks protocol 4. A daemon that predates the `cwd` behavior of `session.new` omits `session_new_cwd_supported`, which decodes as `false`; a daemon that predates `branch_source` and `on_diverged` omits `session_new_branch_source_supported` likewise; a daemon that predates `input_ack_timeout_ms` omits `session_new_input_ack_supported` likewise; a daemon that predates `secret_env` omits `session_new_secret_env_supported` likewise; a daemon that predates package commands omits `package_install_commands_supported` likewise and rejects `accept_commands`; a daemon that predates the expected pin of `package.bind_profile` omits `package_bind_profile_expected_supported` likewise and rejects `expected_previous`. Its `runtimes` entries
are live host-local probes: `agent` is the selected profile or base name and
optional `agent_base` is the `RuntimeRef` of the runtime behind it. Optional
`version` and `supported` are a provider policy, not generic availability:
their absence means that no version policy applies. The daemon builds the
inventory from its runtime registry: a runtime whose definition names a
version-probe parser reports a policy, and a present runtime with a policy
always reports `supported` as `true` or `false`. Clients derive launchability
from the entry alone: `available` and `supported != false`; an entry with no
`supported` has no policy, and no runtime name or `agent_base` is special-cased.
An unavailable runtime with a policy omits both fields.

Optional `config_home_id` on a `runtimes` entry is an opaque, non-secret
identifier of the config home a launch of that entry gives its agent, so a
client can group the entries of one account without learning a path. It is
reported for the bare runtimes that declare a `[config_home]` and for each host
profile of such a runtime, resolved by the same rule as the integration
lifecycle and the transcript observer (the declared variable from the launch
base environment overridden by the profile's `[env]`, else the default below
`HOME`). Two entries carry the same value exactly when their homes are one
directory: the canonical path when the directory exists, else the path with its
`.` components removed (a path with `..` through a missing directory keeps its
own identity), so a symlinked alias shares the id and a home that is created
later may change it. The value is 32 lowercase hex digits, a truncated HMAC of
the directory under the host's `profile-revision.key` secret with its own
domain, so it is stable across calls and daemon restarts of one host, differs
between hosts, cannot be checked against a guessed path without the key and
reveals neither the path nor any environment value. It is safe in a relay or
remote `host.inspect` answer for that reason, and it is not comparable across
hosts. The field is absent for a runtime without `[config_home]` (`shell`,
`hermes`), an unavailable pinned profile, a home that cannot be resolved (a
relative or non-UTF-8 value) and a host whose key cannot be read or created;
`host.inspect` never fails because of it. The first call on a host with a state
directory creates the key record if no relay approval has yet. A daemon that
predates the field omits it. For Hermes (served by its official package),
`available: false` omits both fields, while an installed unparseable or
non-`0.20.0` executable reports `supported: false`. The daemon independently
enforces the same policy immediately before every Hermes launch or recovery
rather than trusting a potentially stale inventory response. The probe clears
ambient environment state except the launch `PATH` and uses private temporary
HOME, XDG, temporary and working directories. Its executable is resolved and canonicalized once;
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
  `fault`, `referenced` (a live, lost or resumable session or a host
  profile pins the digest) and optional `selection_blocked`
  (`{reason, retained}`: why an installed, unselected package cannot be
  selected yet; see "Compatibility-gated selection" below).
- `PackageFault`: `root_missing`, `root_modified`, `root_unsafe`,
  `root_unreadable`, `descriptor_missing`, `descriptor_invalid`,
  `identity_mismatch`, `runtime_not_claimable`, `runtime_conflict`. `fault` is
  absent when the root verified and the descriptor loaded.
- `PackageRuntimeInfo`: `runtime_id`, `display_name`, `program`, `args`,
  `resumable`, `forkable`, `prompt_argument` and the optional `launch_args`
  (the arguments appended at a fresh launch to pass the generated reference),
  `resume_args`, `fork_args`, `fork_launch_args` (the arguments appended after
  `fork_args` to pass a fork's own generated reference, present only for a
  runtime that declares `assign_fork`; all templates in which `{reference}`
  marks the reference slot), `version_probe`, `integration_handler` and `hook_schema` (the
  core-owned handler and hook schema ids of the descriptor's `[integration]`
  table, absent for a runtime without one). The program, the
  fixed arguments and every template are exactly what the daemon can put in
  the argv of a process it launches as the owner (the first prompt is appended
  when `prompt_argument` is true), so all of it can be reviewed before an
  install. A descriptor carries no environment values, setup commands or hooks.
- `CommandInfo`: `name` (`[a-z][a-z0-9-]{0,31}`), `program` (package-relative
  path of a manifest file with the executable bit), `args` (fixed literal
  arguments, at most 64) and `expose` (`local`, or `peers` for every overlay
  peer the overlay policy admits). A descriptor exports at most 16 commands in
  `[commands.<name>]` tables, gated by the runtime feature
  `package-commands-v1`; a call supplies no argument, environment or input of
  its own. `package.install` and `package.link` list them so the owner reviews
  and consents to the program, arguments and exposure before the install. An
  argument may contain Unicode format characters (bidirectional controls,
  zero-width characters); a client that shows arguments for consent escapes
  them, as the CLI does.
  `command.run` calls one (see [Package Commands](#package-commands)).
- `PackageTrust`, a tagged object: `{"kind":"explicit_digest","digest":...}`
  requires the archive digest to equal `digest` and never authorizes an
  official runtime alias; `{"kind":"catalog","catalog_path":...}` names a
  signed catalog at an absolute path on the daemon host.

| Method | Params | Result | Behavior |
|---|---|---|---|
| `package.list` | `null` | `{generation, packages[]}` | `generation` counts committed registry changes. |
| `package.inspect` | `{digest}` | `{package, runtime?, commands?, commands_only?}` | `runtime` is absent when the descriptor cannot be loaded and for a package that exports commands only. `commands` lists the exported `CommandInfo` (name, program, arguments, exposure) and is absent when there are none or the descriptor cannot be loaded, so an owner can review the exposure after install. `commands_only` is `true` when the descriptor loaded and exports commands only; a reply without it says nothing about the descriptor, so a client reads a missing `runtime` as "descriptor unavailable" and never infers a commands-only package. An older daemon omits both fields. |
| `package.doctor` | `{package?}` or `null` | `{generation, findings[]}` | `package` restricts the report to one package id. A finding is `{kind, digest, package?, fault?, referenced}`; `kind` is `fault`, `unregistered_root` (a root on disk without a record; installing the same archive again adopts it after verification), `pinned_not_installed` (a session or host profile pins a digest the registry does not record) or `selection_blocked` (an installed, unselected package that cannot be selected until a retained reference to an incompatible version of the same package is gone; `blocked_by` is that digest). A finding carries `blocked_by?` besides the fields above. An empty list means everything verified. |
| `package.install` | `{archive_path, trust, enable, select, dry_run, accept_commands?}` | `{status, package, runtime?, commands?, reloaded}` | All parameters except `accept_commands` are required. `runtime` is absent for a package that exports commands only (it serves no runtime and cannot be launched as a session); `commands` lists the exported `CommandInfo` and is absent when there are none. `accept_commands` (default `false`, omitted when `false`) is the client's statement that it shows every exported command for consent: a descriptor that declares `[commands]` (command-only or together with a runtime) is refused with `package_commands_client_unsupported` before anything is written, a dry run included, unless it is `true`. Send it only when `host.inspect` reports `package_install_commands_supported`; an older daemon rejects the unknown parameter. A catalog-trusted archive must export a runtime: a package that exports commands only is refused with `package_untrusted`, because a catalog entry binds a runtime id, and an official package that declares commands is refused the same way until the release install paths show commands for consent. `archive_path` is absolute on the daemon host. `status` is `preview` (dry run, nothing changed), `installed`, `already_installed` (recorded with a verified root) or `root_restored` (recorded, root was missing and extracted again). A package that is already recorded keeps its recorded `enabled`, `selected` and origin: `enable`, `select` and `trust` apply to a new record only. With `select: true`, a package whose integration is incompatible with a retained version of the same package is still installed, but unselected: the call succeeds with `package.selected: false` and `package.selection_blocked` set (a dry run reports the same). |
| `package.link` | `{directory, dry_run, accept_commands?}` | `{status, package, runtime?, commands?, reloaded}` | Copies the directory into content-addressed storage and installs it disabled and unselected; the daemon never loads the directory itself. `accept_commands` works as for `package.install`. |
| `package.set_enabled` | `{digest, enabled}` | `{package, reloaded}` | Disabling blocks fresh launches only; a session already pinned to the digest still resumes. Enabling first proves the package root verifies and the package may serve its runtime id, so it cannot create a runtime id conflict. |
| `package.select` | `{digest}` | `{package, reloaded}` | Bare requests of the package id resolve to this version afterwards. The same root and runtime id checks as enabling apply. Refused with `package_integration_incompatible` while a retained reference uses a version of the same package whose integration differs; nothing changes. |
| `package.uninstall` | `{digest, remove_modified}` | `{digest, reloaded}` | Refused with `package_referenced` while a session or host profile pins the digest, and with `package_retention_uncertain` while a `metadata.jsonl` line that may pin a package cannot be read (the line is kept; list it with `store.rejected_list` and move it with `store.rejected_set_aside`). `remove_modified: true` removes only a root that fails verification; a verified root is refused with `package_root_intact`. |
| `package.bind_profile` | `{profile, digest?, expected_previous?, dry_run}` | `{status, profile, base, previous?, package, runtime, reloaded}` | `profile` is the file name under the agents directory without `.toml`. `expected_previous` is the pin the caller expects the profile to hold now: `"unpinned"` or `{"pinned": {"digest": "sha256:..."}}`. When the profile's current pin differs (a profile with only one of its `package` and `digest` keys holds no pin), the call is refused with `package_profile_pin_changed` before the target is resolved and nothing is written, a dry run included; absent means no condition. Send it only when `host.inspect` reports `package_bind_profile_expected_supported`; an older daemon rejects the unknown parameter. The comparison reads the same file text the write replaces, so a pin made between the caller's read and the write is refused, not overwritten. With `digest` the installed package of that digest must serve the profile's base runtime and load without a fault; without it the target is the one selected, enabled package serving the base. Only the `package` and `digest` keys are rewritten, every other byte is kept, and the new file replaces the old with one `rename(2)` so the profile name never stops resolving; a profile edited after it was read is left as found. `status` is `preview` (dry run, the pin would change), `bound` or `unchanged` (already pinned; nothing written). `previous` is the digest pinned before. `reloaded` is `true` only for `bound`: profile files are read at every resolution, so the pin is live at once. Runs under the same exclusive authority as `package.uninstall`, so an uninstall of the target either precedes the bind (`package_profile_target_invalid`) or is refused with `package_referenced`. A pin of a version whose integration differs from the selected version of its package, or from another referenced version (a session or another profile's pin; the pin being replaced does not count), is refused with `package_integration_incompatible`, because the pin would be a retained reference that coexists with an incompatible selected version. |

Compatibility-gated selection. An integration handler owns one active asset set
per runtime, and sessions launched from an older version keep reporting through
that version's handler and hook schema until they end. The integration of a
package is the pair of its handler id and hook schema id, or none. Two versions
of one package are compatible when their integrations are equal: every compiled
handler drives exactly one schema and the registry declares no coexistence
between different pairs, so any difference, including one side having no
integration, is incompatible. A version whose integration cannot be determined
(its root or descriptor is unreadable) is treated as incompatible; a descriptor
that predates `hook_schema` resolves through the schema its handler drives. A
package is *held back* while a retained digest of the same package (a live, lost
or resumable session, or a host profile pin; the same set that blocks
`package.uninstall`, widened to every installed version while an unreadable
`metadata.jsonl` line may pin a package) has a different integration: `package.select` is refused
with `package_integration_incompatible`, `package.install` with `select: true`
installs it unselected, and the selected version keeps serving, with no fallback
to another package. A retained pin still resolves from exactly its pinned digest.
The state is not stored: `package.list`, `package.inspect` and `package.doctor`
derive `selection_blocked` from the registry and the retained set at the moment
of the call, so it survives a daemon restart and clears on the first listing
after the last such reference is gone (session removed, profile unpinned or
rebound, old version uninstalled). Nothing selects the package then; the owner
selects it explicitly. Selection changes commit under the exclusive lifecycle
authority, and a launch that resolved the previously selected version before a
selection committed is refused with `runtime/runtime_package_changed` (retry the
request) instead of pinning the version that was just replaced; a digest a host
profile pins launches whatever is selected. While an unreadable `metadata.jsonl`
line may pin a package, every installed version counts as retained, including
the pin a `package.bind_profile` rebind replaces.

Install and link validate the package in memory before anything is extracted:
`runtime.toml` is parsed from the verified archive, and the package identity
(id and version) comes from that descriptor. `shell` is never claimable. The
official aliases (the reserved runtime ids other than `shell`) are served only
by a package the signed catalog authorizes as official for that exact package
id, runtime id and archive digest; such a package serves the alias while it is
installed, enabled and selected. The daemon compiles none of Codex, Claude Code and Hermes in, so
`codex`, `claude` and `hermes` are each served by the official package or not
at all, and disabling or uninstalling the package leaves the alias unserved: a
host without it answers a bare `codex`, `claude` or `hermes` with
`agent_profile_not_found`, an unpinned host profile over it with
`runtime_not_installed` (`runtime_incompatible` with a doctor recovery while the
installed official package of that runtime, `pohunek.runtime.<runtime>`, cannot
be loaded), and `integration.install` for `codex` or `claude` with
`runtime_not_installed`, each with a recovery hint naming
`pohunek plugin install runtime/packages/<runtime>.tar.zst --catalog
runtime/runtime-catalog.json` from the extracted release archive (and, for the
profile, `pohunek plugin profile migrate <name>` after it). `integration.status`,
`integration.doctor` and `integration.uninstall` that name `codex` or `claude`
work without the package: the core-owned handler keeps the config home of the
earlier built-in (`CODEX_HOME`, else `~/.codex`; `CLAUDE_CONFIG_DIR`, else
`~/.claude`), so hooks pohunek installed there are reported and removed (only
the entries it owns); `profile` and `all_profiles` reach the homes of host
profiles over that runtime through each profile's own `[env]`, although such a
profile does not resolve; without `agent`, status and doctor cover only served
runtimes. A runtime id served by a
loaded package of a different package id is a conflict. A claim of an alias by
an explicit-digest or linked package is answered with
`package_runtime_not_claimable`, and so is a descriptor that names `shell`; a
catalog that names `shell` or does not authorize the alias for the archive is
`package_untrusted`. The package that serves an alias is the selected, enabled
official package whose `runtime_id` is that alias in `package.list` and
`package.inspect`; no such package means nothing serves it.

A session launched from the built-in runtime of an official alias this daemon
does not compile in (the built-in Codex, Claude Code or Hermes of an earlier
release, pinned or from a record without a launch binding) while nothing serves
the alias is refused with `runtime/runtime_builtin_removed` on resume, fork and
every mutation (input, attach, resize, rename, metadata); only stop and remove
still work. Its recovery hint names the official package install and the
new-session fallback. Once the official package serves the alias, it adopts a
session whose launch pin names that package id, live or stopped; the package
version may differ. The adopted session accepts input again, and resume and fork
run through the package when it honors the session's recovery contract (the same
native reference kind, a hook-reported reference, and for fork, fork support in
both the frozen spec and the package). The relaunched run is pinned to the
package digest. A session whose contract the package cannot honor, and a
session record that names no package, are refused with
`runtime/runtime_served_by_package`, whose recovery hint names the one
migration (start a new session with `pohunek session new --agent <runtime>`,
which the official package serves; no command rebinds the session to another
runtime source), and the stored binding is kept unchanged. A fork of a Hermes
session answers `agent_fork_unsupported`, because Hermes declares no native fork.
`session list` and `session inspect` report each of resume and fork as false,
with its refusal code in `capability_refusals`, when the session's recovery
resolution refuses that operation. A session pinned to a runtime
source the daemon does not serve otherwise (a built-in of a runtime id that is
not an official alias, a package digest that is not installed) is refused with
`runtime/runtime_not_installed`. Stop and remove still work. A session the
daemon launched keeps the definition it was launched from while that daemon
runs. A built-in pin that the installed official package adopts is served by
that package, live or stopped. A session adopted at startup whose pin the daemon
does not serve is observed only through its launch-time facts: the generic shell manifest (a
manifest override of the session's host profile still applies), the input
framing persisted at launch, and the hook schema its worker journaled. Hook
reports and processes naming its own runtime are not attributed through the
package that serves the runtime id now: `session.report_agent` and
`session.release_agent` for that runtime are ignored (logged at INFO as
`session.callback.unserved_pin`), and the public socket admits a hook report (a
notification, for example) only by the schema the session's worker journaled,
the rule the worker applies on its own socket. This holds until a daemon
restart adopts the session again, also when the pinned source is served again
in between (the package uninstalled, for example). A recovery binding whose
launch spec the store migration left unresolved is completed only from the
definition its pin names; while the pin is not served it stays unrepaired and
the session carries a `native_recovery` warning.
A worker of a release before 0.34.0 journals no hook schema, so a live session
whose worker journaled an active identity on such a pin cannot have it verified:
the upgrade preflight reports it `would_not_be_adopted` with
`worker_identity_unverified`, and startup does not adopt it (the identity import
fails with `active_identity_provider_invalid` and the runtime is reported as a
`conflict`).
Catalog trust fails closed with
`official_trust_unavailable` on a host without a trust anchor and with
`official_trust_anchor_invalid` on a host whose anchor cannot be trusted. The
daemon reads the anchor once at startup from `runtime-catalog-anchor.json` in
the directory of its executable (public keys, validity windows and revoked key
ids; see the runtime catalog concept page); the file must be a regular,
non-linked, size-bounded file that only the daemon's user or root can write,
under directories only they can write. `daemon.doctor` reports the state as the
`catalog_trust_anchor` check: `ok` when loaded, `warn` when absent, `fail` when
untrusted.

A catalog install checks the catalog signature, the persisted high-water
sequence and revoked key ids, then that an entry binds the package id, runtime
id, version and digest and supports this core version and platform; before the
package is published it persists the catalog's sequence and revocations, and a
failure to do so is answered with `package_registry_failed` with nothing
installed, enabled or reloaded.

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
| `package_untrusted` | `runtime` | The trust does not authorize the archive, including a catalog-trusted archive that exports commands only (a catalog entry binds a runtime id). |
| `package_commands_client_unsupported` | `runtime` | The descriptor declares `[commands]` and the request did not set `accept_commands`; nothing was recorded, a dry run included. |
| `official_trust_unavailable` | `configuration` | The host has no catalog trust anchor, so an official package cannot be authorized. |
| `official_trust_anchor_invalid` | `configuration` | The host's catalog trust anchor exists but cannot be trusted (unsafe location, permissions or ownership, or invalid content), so an official package cannot be authorized. |
| `package_incompatible` | `runtime` | The package does not support this core version or platform. |
| `package_descriptor_invalid` | `runtime` | The package's runtime descriptor is not a valid runtime definition. |
| `package_runtime_not_claimable` | `runtime` | The package claims a runtime id it may not serve. |
| `package_runtime_conflict` | `runtime` | Another package or a built-in runtime serves the runtime id. |
| `package_not_installed` | `runtime` | The digest is not installed. |
| `package_identity_installed` | `runtime` | The package id and version are installed from another archive. |
| `package_identity_conflict` | `runtime` | The digest is installed under another package identity. |
| `package_referenced` | `runtime` | A session or host profile still pins the digest. |
| `package_retention_uncertain` | `runtime` | A `metadata.jsonl` line that may pin a package cannot be read, so `package.uninstall` cannot prove the digest unreferenced. The line stays in the store until `store.rejected_set_aside` moves it into the quarantine file. Listing, inspecting and doctor still work; selecting, binding and integration updates judge every installed version while the line stays unreadable. |
| `package_root_invalid` | `runtime` | The recorded package root fails verification. |
| `package_root_intact` | `runtime` | The recorded package root verifies, so it is not removed as modified. |
| `package_registry_busy` | `daemon` | Another writer holds the package registry. |
| `package_registry_failed` | `runtime` | The package registry or its storage failed. |
| `package_limit_reached` | `runtime` | The registry holds the maximum number of packages. |
| `package_reload_failed` | `daemon` | The change was committed but the runtime registry was not rebuilt. |
| `package_profile_not_found` | `runtime` | The host has no agent profile of that name. |
| `package_profile_unusable` | `runtime` | The profile fails the daemon's profile acceptance rule, is a symbolic or hard link, or is not valid TOML. |
| `package_profile_base_builtin` | `runtime` | The profile's base runtime is the built-in `shell`, so there is no package to pin. |
| `package_profile_target_invalid` | `runtime` | The package is not installed, is faulted, does not serve the base runtime, or is not the unique selected package. |
| `package_profile_changed` | `runtime` | The profile changed while it was being rewritten and was left as found. |
| `package_profile_pin_changed` | `runtime` | The profile's current pin is not the one `expected_previous` named; nothing was written. |
| `package_integration_incompatible` | `runtime` | The package's integration handler or hook schema is not compatible with another version of it that is still referenced or selected, or that an unreadable `metadata.jsonl` line may reference; nothing changed. |
| `runtime_feature_unsupported` | `runtime` | The descriptor's `requires` names a runtime feature this core does not implement, or the descriptor uses a key or compiled parser or handler id of a feature it does not list in `requires`; nothing was recorded. |

### Rejected Store Lines

A `metadata.jsonl` line no loader accepts (a record of a newer or damaged
daemon) is kept in the file through every rewrite, and package gates fail
closed while a line that may pin a package stays (`package_retention_uncertain`).
`store.rejected_list` and `store.rejected_set_aside` are the supported way to
inspect and remove such lines. Both are served on the local control socket only:
a remote connection is refused with `local_only_method` before a parameter is
read. A protocol 3 connection gets `method_not_found`, as for every method added
after protocol 3.

| Method | Params | `ok` result | Notes |
|---|---|---|---|
| `store.rejected_list` | `null` | `{lines[], total}` | Each line is `{line, kind?, session_id?, reason, may_pin, may_own_runtime, content_hash}`. `line` is the 1-based position in `metadata.jsonl`; `kind` and `session_id` are the line's own text and, like `reason`, cut to `STORE_REJECTED_TEXT_MAX_BYTES` (128). `may_pin` is true when the readable `kind` is not `worktree` or `project` and the line does not decode as a record, so the pin it may hold cannot be read; a line that decodes but names an agent kind this build does not know has `may_pin` false and its pin counts like a loaded record's. `may_own_runtime` is true for a line whose session state cannot be read, including a line that is not a JSON object, and for a `resume` line whose pin cannot be read (`may_pin`): the session it resumes may be running. `content_hash` is the lowercase hex SHA-256 of the line without its terminator. The raw line is never returned. At most `STORE_REJECTED_LIST_MAX_LINES` (100) lines are returned; `total` counts all of them. |
| `store.rejected_set_aside` | `{content_hash, accept_pin_loss?}` | `{content_hash, lines[], quarantine_file}` | Moves every rejected line with this hash, in one call, into `metadata.rejected.jsonl` next to the store. `lines` are the numbers the lines had before the move; `quarantine_file` is a file name, not a path. A line with `may_own_runtime` is refused with `store_rejected_line_live` unless `accept_pin_loss` is true, because dropping it drops the package pin that protects a root a live worker may run from. Params are strict: an unknown field is `bad_request`. |

The quarantine file is owner-private (`0600`) JSON lines, written through an
atomic replace before the store is rewritten without the lines, both under the
store's write lock; a crash between the two steps leaves the line in both files
and a repeat appends a second record. Each record is
`{quarantine_schema, set_aside_at, line, content_hash, kind?, reason, may_pin, may_own_runtime, accepted_pin_loss, text}`;
`text` is the line exactly as stored. The daemon reads the file only to append
to it, never interprets or deletes it, and a quarantine file that is not UTF-8,
not owner-private or over the store size limit fails every later set-aside with
`store_quarantine_failed`. The other records are rewritten as any store write
rewrites them: grouped by kind, blank lines dropped, and the lines that stay
rejected kept as stored after them.

| Code | Class | Meaning |
|---|---|---|
| `store_rejected_line_not_found` | `runtime` | No rejected line has the content hash (it may already have been set aside); list again. |
| `store_rejected_line_live` | `runtime` | The line may own a live runtime and `accept_pin_loss` was not set; nothing changed. |
| `store_quarantine_failed` | `daemon` | The quarantine file or the store rewrite failed; the store still holds the line. |
| `store_read_failed` | `daemon` | The store cannot be read or its schema is refused. |

### Package Commands

`command.run` runs one command a package declared in its descriptor (see the
[runtime package guide](knowledge/guides/runtime-packages.md#declaring-package-commands)).
The call names a package id and a command; it carries no argument, environment
variable, working directory or input.

| Method | Params | `ok` result | Notes |
|---|---|---|---|
| `command.run` | `{package, command, timeout_ms?, max_output_bytes?}` | `{package, command, digest, output, duration_ms}` | Params are strict: an unknown field (`args`, `env`, `cwd`, `stdin`) is `bad_request` and nothing starts. `package` is the package id; the call resolves to the selected version, so `package` in the result is the id and exact version that ran and `digest` names its archive. `timeout_ms` and `max_output_bytes` are 1 to `MAX_COMMAND_TIMEOUT_MS` (60000) and 1 to `MAX_COMMAND_OUTPUT_BYTES` (128 KiB); absent means the ceiling, a larger value is lowered to it and zero is `bad_request`. `output` is the program's stdout verbatim and valid UTF-8. |

The daemon runs the declared program once, as the owner:

- the program is a file of the package root, which is verified against the
  archive digest again before every run; the arguments are the descriptor's
  literals;
- the environment is cleared and holds only `PATH` (the one agent launches
  inherit), `HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`,
  `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `TMPDIR`, `LANG=C` and `LC_ALL=C`; the
  home and XDG paths are a private scratch directory removed after the run,
  which is also the working directory; stdin is closed and stderr is discarded;
- the program runs in a process group of its own. The group is killed when the
  deadline passes, when stdout exceeds the cap (checked while the program
  runs), after the program exits (so a background child does not survive) and
  when the daemon shuts down or the caller disconnects. A program that calls
  `setsid()` or double-forks leaves the group and lives until the daemon's unit
  stops;
- one run per package command and four runs across the host at once, over every
  transport: a call over a limit fails with `command_busy` at once and nothing
  queues. A run keeps its slot until its process group is gone;
- a call resolves in this order: package installed, version selected, command
  declared, exposure, enabled, root verified, limits, run. The exposure gate
  reads the commands from the descriptor the daemon holds for the selected
  digest and is decided by the transport of the connection: the local socket
  may call any declared command, an overlay connection only a `peers` command
  (`command_not_exposed` otherwise). No program path is built and nothing
  starts before it. The method is not served on a relay path.

Errors are fixed text without a path, a value or program output:

| Code | Class | When |
|---|---|---|
| `command_package_not_installed` | `runtime` | No version of the package is installed. |
| `command_not_selected` | `runtime` | Versions are installed and none is selected. |
| `command_not_declared` | `runtime` | The selected version declares no command of that name, or the package exports no commands. |
| `command_not_exposed` | `runtime` | An overlay caller named a `local` command. |
| `command_package_disabled` | `runtime` | The selected version is disabled. |
| `command_busy` | `runtime` | The command already runs, or four runs are in flight. |
| `command_failed` | `runtime` | The program exited non-zero or was ended by a signal the daemon did not send; `details.process_exit` carries `exit_code` or `signal`. |
| `command_timed_out` | `runtime` | The deadline passed and the group was killed. |
| `command_output_too_large` | `runtime` | Stdout exceeded the cap and the group was killed. |
| `command_output_not_utf8` | `runtime` | Stdout is not valid UTF-8. |
| `command_unavailable` | `runtime` | The package root fails verification, the run could not be set up, or the daemon is shutting down. |
| `command_unsupported` | `daemon` | Produced by a client, never by a daemon: the daemon answered `method_not_found` (a release without `command.run`, protocol 4 included) or speaks protocol 3, which never defined it. |

The log line of a run names the package, version, digest, command, transport,
duration, outcome, exit status or signal and the number of output bytes, never
the output.

### Daemon Runtime Configuration

`POHUNEK_OBSERVE_EXTERNAL_AGENTS` is an opt-in daemon environment flag. Accepted
true values are `1`, `true`, `yes`, and `on`; accepted false values are `0`,
`false`, `no`, `off`, or an unset variable. When true, the daemon watches the
operator's Claude and Codex transcript trees and same-user process table for
agents started outside pohunek. The transcript trees are the `projects` (Claude)
and `sessions` (Codex) directories of every config home the host launches those
agents with: each runtime's own home and the home of every host profile of it,
resolved as a launch resolves it (see "Config home"). The set is re-read on every
reconciliation pass, so a profile added, edited or removed is followed without a
daemon restart, and homes that resolve to one canonical directory are watched
once. A profile whose home does not resolve or has no transcript tree yet is
skipped and logged once at `debug`; a tree that cannot be inspected (permissions,
I/O error) stays a root, keeps its indexed transcripts and degrades the watcher; a transcript below a root that left the set
leaves the index. An observed external session is still matched to a candidate by
agent base alone, never attributed to a profile. The directory of a profile-derived
home is private: an external session matched to a transcript below one omits
`native_session_path`, and the watcher's log lines name such a root by a label,
not its path. The daemon's own process
environment steers nothing unless the launch allowlist forwards the variable. The
corresponding `SessionRegistryConfig` setting is `observe_external_agents`,
default `false`.

Observation limits are validated together when the session registry starts.
Defaults are 783,240 raw output bytes, an 8,000 ms output wait, an 8,000 ms
session wait, 200 rows, 500 columns, a 1,048,418-byte serialized screen result,
128 global waiters, and 8 waiters per session. Turn waits have their own limits:
a 5-minute default duration, a 30-minute ceiling, 128 global and 8 per-session
turn waiters, and a 1,024-event journal. Values must be non-zero, must not
exceed the shared protocol ceilings (the turn-wait ceiling at most
`MAX_SESSION_WAIT_TURN_MS`, one day), the per-session waiter caps must not
exceed their global caps, and the turn-wait default must not exceed its
ceiling. Invalid combinations fail fast with
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
- `local`: optional boolean, serialized only when `true`. The record is the
  machine that enumerated the hosts, not a peer; its `peer_id` is `null` and its
  class comes from a health probe of its own overlay address. A reader that
  predates the field ignores it and shows the record as one more host.
- `reason`: optional string, serialized only when set. A `local` record whose
  probe failed carries the probe error next to `unreachable`; it is never set on
  a peer record.
- `name` and `fqdn`: optional display selectors, never sufficient by themselves
  to bypass collision checks.
- The flattened host class remains `candidate`, `reachable_daemon`,
  `unreachable`, or `version_mismatch`. A peer is `reachable_daemon` when it
  answers a health probe inside the protocol range the asking client advertised
  (a current Rust client or CLI: `3..=4`), and `version_mismatch` when it answers
  outside it. `host.discover` classifies for the range of the asking request.

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
| `hook` | a validated integration report (process-bound, ancestry, sequence and expiry checked) | on once a report arrived; requires a supported resume; may declare `[native_reference.existence]` for an id-kind reference |
| `assigned` | generated by core and passed to the agent at launch (and at fork with `assign_fork = true`) | on from the first moment; requires a supported resume |

The built-in `shell` declares `none`; the official Codex, Claude Code and
Hermes packages declare `codex`, `claude` and `hermes` = `hook`.

### Config home

A runtime may declare where its agent keeps its configuration, hook registration
and conversations, apart from `[integration]` because the home concerns every
runtime:

```toml
[config_home]
env = "CLAUDE_CONFIG_DIR"
default = ".claude"
```

`env` names the variable the agent reads to relocate its home (upper-case, not
in the reserved `POHUNEK_` namespace) and `default` is the directory below the
user's home used while the variable is unset or empty (a relative path of plain
components: no `..`, no absolute path, no glob or shell characters, bounded).
Both keys are required and unknown keys are refused (`descriptor_invalid`). The
daemon-run integration handlers install into this home, so a runtime that names
one needs the table. The official Codex and Claude Code packages declare it;
any package may too. Pi keeps declaring its conversation root under `[native_reference.existence]`.

An agent whose variable holds a parent of its home, such as `XDG_CONFIG_HOME`,
declares the optional `subdir`, a directory below the variable's value (runtime
feature `config-home-subdir-v1`, so the descriptor lists it in `requires`; see
"Runtime features" in the runtime packages guide):

```toml
requires = ["config-home-subdir-v1"]

[config_home]
env = "XDG_CONFIG_HOME"
subdir = "muse"
default = ".config/muse"
```

`subdir` follows the `default` rule (a relative path of plain components,
bounded) and joins the variable's value only: while the variable is set the
home is `$XDG_CONFIG_HOME/muse`, and while it is unset or empty the home is
`default` below `HOME`, which stays the complete path. A relative or non-UTF-8
variable value is a typed error (`agent_config_dir_invalid`) and never falls
back to `default`, unlike the XDG specification. Two runtimes that reach one
directory through one integration handler are ambiguous by their resolved path,
not by `env`, so the same `env` with different `subdir` values is not.

### Launch environment

A runtime may declare flag environment variables its launches and its
version probe get (runtime feature `launch-env-v1`, listed in `requires`):

```toml
requires = ["launch-env-v1"]

[launch_env]
ACME_NO_AUTO_UPDATE = "1"    # runtime id `acme`
```

The table holds at most 16 string entries and its pairs are behaviour flags.
A name must start with the runtime's own namespace, derived from `runtime.id`:
upper case, `-` and `.` mapped to `_`, then `_` (`muse` gives `MUSE_`), with at
least one character after it. The core allowlist outside the namespace is empty.
A runtime whose namespace begins with a refused prefix, so a variable of the
loader, the owner's tools or an interpreter, package-manager, privilege or
network tool the agent runs (`BASH_ENV`, `NODE_OPTIONS`, `SUDO_ASKPASS`,
`PYTHONSTARTUP`, `CURL_CA_BUNDLE`) cannot be set by a descriptor, has no usable
`[launch_env]`. The prefixes are `POHUNEK_`, `LD_`, `DYLD_`, `XDG_`, `LC_`,
`SYSTEMD_`, `DBUS_`, `WAYLAND_`, `DISPLAY_`, `SSH_`, `GPG_`, `GNUPG`, `AWS_`,
`AZURE_`, `GOOGLE_`, `GCLOUD_`, `CLOUDSDK_`, `OCI_`, `ALIBABA_`, `ALICLOUD_`,
`DIGITALOCEAN_`, `HCLOUD_`, `TERRAFORM_`, `TF_`, `VAULT_`, `ANSIBLE_`, `GIT_`,
`GH_`, `GITHUB_`, `GITLAB_`, `DOCKER_`, `KUBE`, `HELM_`, `PODMAN_`, `BUILDAH_`,
`CONTAINER_`, `CONTAINERS_`, `CONTAINERD_`, `COMPOSE_`, `HTTP_`, `HTTPS_`, `FTP_`, `ALL_`, `NO_`,
`SOCKS`, `CURL_`, `WGET_`, `REQUESTS_`, `SSL_`, `OPENSSL_`, `GNUTLS_`, `NSS_`,
`SSLKEYLOGFILE`, `BASH_`, `ZSH_`, `SH_`, `ZDOTDIR`, `SUDO_`, `NODE_`, `NPM_`,
`YARN_`, `PNPM_`, `JAVA_`, `JDK_`, `PYTHON`, `PIP_`, `PERL`, `RUBY`, `GEM_`,
`CARGO_`, `RUST`, `GO_`, the Go variables `GOFLAGS`, `GOPATH`, `GOROOT`,
`GOPROXY`, `GONOSUMDB`, `GONOPROXY`, `GOSUMDB`, `GOINSECURE`, `GOPRIVATE`,
`GOTOOLCHAIN`, `GOCACHE`, `GOMODCACHE`, `GOENV` and `GOWORK`, `CGO_`, `LUA_`,
`DOTNET_`, `NUGET_`, `MSBUILD`, `PHP_`, `PHPRC`, `COMPOSER_`, `BUNDLE_`,
`CONDA_`, `UV_`, `BUN_`, `DENO_`, `GRADLE_`, `MAVEN_`, `GLIBC_`, `MALLOC_`,
`NIX_`, `GUIX_`, `HOMEBREW_`, `PYENV_`, `RBENV_`, `NVM_`, `VOLTA_`, `ASDF_`,
`MISE_`, `RSYNC_`, `CCACHE_`, `SCCACHE_`, `CMAKE_`, `ELECTRON_`, `SVN_`,
`BORG_`, `RESTIC_`, `HEX_` and `R_`. A prefix without a trailing `_` (such as `PYTHON`, `RUST` or `GOPATH`)
matches any runtime id that starts with those letters (`python3`, `rustc`,
`gopath-agent`); `goose` is unaffected, because only `GO_` and the exact Go
variable names are listed. Entries such as `GOFLAGS` or `PHPRC` therefore
reserve runtime ids; no declarable name can equal them. The list grows only
through an issue naming a concrete `<NAMESPACE>_*` variable whose value from the
closed set below runs code, turns off verification, sets a credential or names a
file. Inside the namespace a name is refused when it is
a service-manager variable the worker never passes to an agent
(`NOTIFY_SOCKET`, `INVOCATION_ID`, ...), the variable the same descriptor reads
its `[config_home]` or `[native_reference.existence]` root from, secret-shaped
(any `_`-separated word after the namespace is `KEY`, `KEYS`, `TOKEN`, `TOKENS`,
`SECRET`, `SECRETS`, `PASSWORD`, `PASSWD`, `PASSPHRASE`, `PASSPHRASES`,
`CREDENTIAL`, `CREDENTIALS` or `AUTH`), or location-, identity- or code-shaped (any such word is `PATH`,
`PATHS`, `DIR`, `DIRS`, `DIRECTORY`, `FOLDER`, `HOME`, `ROOT`, `FILE`, `FILES`,
`SOCK`, `SOCKET`, `SOCKETS`, `URL`, `URLS`, `URI`, `HOST`, `HOSTS`, `SERVER`,
`ADDR`, `ENDPOINT`, `REGISTRY`, `PROXY`, `COMMAND`, `CMD`, `BIN`, `BINARY`,
`EXECUTABLE`, `SCRIPT`, `OPTS`, `OPTIONS`, `ARGS`, `FLAGS`, `EXEC`, `SHELL`,
`EDITOR`, `HOOK`, `HOOKS`, `PLUGIN`, `PLUGINS`, `MODULES`, `LIB`, `LIBS`,
`PRELOAD`, `CERT`, `CERTS`, `CA`, `KEYRING`, `PROFILE`, `CONF`, `CONFIG` or
`ID`, or a longer word ending in `PATH`, `DIR`, `FILE`, `HOME`, `ROOT`,
`FLAGS`, `OPTS`, `CONF`, `CERT` or `CERTS`, such as `TMPDIR` or `LIBPATH`; `PI_PATHOLOGY`
is fine and `PI_LOG_DIR_MODE` is not; these word lists are a second layer, not
a complete vocabulary, because `0` and `1` are also valid file names), so a
public descriptor cannot redirect or blank owner or core state. Each value is
exactly `0`, `1`, `true` or `false` (case-sensitive, never empty, never
expanded); a program, path, host, port or credential cannot be a value, and
widening the set needs a new feature id. A refusal is `descriptor_invalid` and names the field, never a
name or a value. The pairs are not persisted: each launch (a new session, a
resume, a fork) reads them from the definition that serves it. A launch applies
them first, then the session's private `TMPDIR`, then the host profile's
`[env]`, the runtime contributor's pairs, the reserved `POHUNEK_*` values, then
the request's `secret_env` values. A slot that collides with an earlier layer
is refused before the worker socket, the record or any worktree exists, and the
outcome depends on the layer: a slot naming a variable of the profile's `[env]`,
the session's `TMPDIR` or a reserved `POHUNEK_*` name fails when the profile
loads (`invalid_profile`, naming the slot); a slot naming a runtime
contributor's variable fails the launch with `invalid_profile` naming the slot;
a slot naming a descriptor `[launch_env]` variable or the runtime's
config-home variable fails at match with `secret_env_not_declared`. The worker
re-asserts `POHUNEK_*` last (a slot can never name one). The version probe applies the descriptor pairs after clearing its
environment; a profile entry reaches the launch only. A program the user starts
by hand inside a shell session gets no descriptor environment. A built-in
descriptor digest covers the table only while it holds a pair.

### Secret session environment

A host profile can declare variables whose values come from the `session.new`
request instead of the profile file, for a credential that differs per session
(a metering token) and must not be stored on the host:

```toml
# agents/metered.toml
base = "shell"
secret_env = ["METER_TOKEN"]
```

The profile owns the names and the request supplies the values, so a remote
caller cannot choose which variable the agent process gets. Every declared name
is required on every launch of the profile: a launch without all of them fails
with `secret_env_missing`, so the agent never falls back to credentials it finds
on the host. A slot is checked when the profile loads and a bad one makes it
`invalid_profile` (the message names the key, never a value): a portable
upper-case environment name, at most 16 of them, none twice, none starting with
`POHUNEK_`, `LD_` or `DYLD_`, none on the service-manager denylist, none of
`TERM`, `TMPDIR`, `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL` or `LANG`, and none
that is also a key of the profile's `[env]`. When a request is matched, a slot
equal to the runtime's own `[config_home]` variable or one of its `[launch_env]`
names cannot be filled (`secret_env_not_declared`). Values are 1 to 16384 bytes
without NUL, at most 16 entries.

The values travel in the request and reach the agent once, in the worker's
private `Initialize` message; the daemon does not persist them. They appear in
no `session.list`, `session.inspect`, event, notification, metadata, journal,
log or error text, and the typed parameter prints only an entry count. Because
nothing is kept, `session.resume` and `session.fork` of a session whose profile
declares slots fail with `secret_env_missing`, and so does a relay-origin launch
of such a profile. A slot name is a code-execution decision of the profile owner: declaring an interpreter hook such as `NODE_OPTIONS`, `BASH_ENV` or `GIT_SSH_COMMAND` lets every caller that may start the profile run code. `"secret_env": null` is refused with `secret_env_invalid`; omit the field instead. The agent process holds the values in its environment for its
lifetime, like a profile `[env]` value: same-user readers of
`/proc/<pid>/environ` and the agent's own children see them.

### Transcripts

A runtime may declare where below its config home the agent writes JSON Lines
conversation transcripts and which compiled parser reads them; the external
observer watches that tree of every config home the host launches the runtime
with:

```toml
[transcripts]
parser = "jsonl-head-v1"
dir = "sessions"
```

`parser` is a closed set compiled into core (`jsonl-head-v1`: the first session
id, transcript path and absolute working directory in the first 32 records or
64 KiB, with `session_id` before `sessionId`, `conversation_id` and
`conversationId`). `dir` follows the `[config_home]` `default` rule (relative,
plain components, no `.` or `..`, no empty segment, bounded). The table requires
`[config_home]`; an unknown parser, an unsafe `dir`, a missing or unknown key,
or a missing `[config_home]` is `descriptor_invalid`. The official Codex
package declares `sessions` and the official Claude Code package declares
`projects`; a runtime without the
table contributes no transcript root. With the runtime feature
`transcripts-head-paths-v1` (listed in `requires`), `jsonl-head-v1` also takes
`id_path` and `cwd_path`, each a list of one to three plain object keys
(`A-Za-z0-9_`, at most 32 bytes each), and reads the native session id and the
absolute working directory at those paths of the file's first line only; a
top-level key of any record wins, and the official Codex package declares
`["payload", "id"]` and `["payload", "cwd"]`. A longer, empty or unsafe path, a
non-list value or another parser is `descriptor_invalid`; a path without the
feature is `runtime_feature_unsupported`. Trees of two different runtimes that are
the same directory or nested in each other are attributed to neither; the
daemon logs the warning `external.transcript_roots.ambiguous` with code
`transcripts_ambiguous_owner`.

#### Transcripts outside the config home

A runtime that keeps its session logs outside the config home names the root
itself (runtime feature `transcripts-data-root-v1`, listed in `requires`):

```toml
requires = ["transcripts-data-root-v1"]

[transcripts]
parser = "muse-session-v1"
root_env = "XDG_DATA_HOME"
root_home = ".local/share"
dir = "muse/sessions"
max_depth = 4
```

`root_env` and `root_home` come together and follow the `env` and `default`
rules of `[config_home]` and its resolution (the variable of a host profile's
`[env]` over the daemon's base environment, else the default below `HOME`; a
relative value is `agent_config_dir_invalid`); with them the table needs no
`[config_home]`. `max_depth` (1 to 6) is the number of directory levels below
`dir` the observer lists and watches. A date-sharded tree is never walked below its session
directories. `muse-session-v1` reads only a log at
`YYYY/MM/DD/<uuid>/session.jsonl` below `dir`, framed (`children[].record_json`)
or bare, within the first 32 lines (64 KiB): the first
`runtime.session.metadata` record gives the session id (`stream.id`, equal to
the `<uuid>` directory name) and the absolute working directory
(`payload.record.workspace_root`). Logs nested below a session and logs at
other depths are not sessions. A pass visits the highest directory names first,
so the newest sessions are indexed when a tree outgrows the per-pass entry
bound. On Linux the watcher keeps a live watch on every date directory and on
the newest 1024 session directories of each dated root (newest first; a new
one evicts the watch registered longest ago), and the periodic pass reaches the older sessions (a change event the backend
reports for a session log indexes it, wherever the pass stopped), so a large store
never takes the backend-wide watch budget from other runtimes. These keys are refused without the feature
(`runtime_feature_unsupported`), and a `root_env` or `root_home` without its
partner, an unsafe path or a `max_depth` out of range is `descriptor_invalid`.

A hook runtime whose conversation is `<reference>/<file_name>` below a
date-sharded tree declares `dir_name = "{reference}"` in
`[native_reference.existence]` (same feature) next to `check = "file"`,
`root_env`/`root_home`, `dir`, a plain `file_name` and `max_depth`, the number
of levels the scan descends below `dir`; the named directory is an entry of
the directory reached after that many levels (`dir/YYYY/MM/DD` for 3). It is accepted only
with `strategy = "hook"` and an id-kind reference and without `name_match`,
`store_env`, `store_max_depth` or `relocating_*`. Recovery returns
`agent_native_reference_missing` when no regular `<reference>/<file_name>`
exists there; no symlink is followed. The declaration is not stored with a
session, so the stored shape of an assigned reference is unchanged.

### Hook schemas

A runtime that reports lifecycle hooks (identity, release, notification,
subagent) declares an integration in its descriptor. The package names only
the compiled handler and the hook schema its reports follow; it never carries
schema contents:

```toml
[integration]
handler = "codex-hook-v1"
hook_schema = "identity-subagent-v1"
```

Both keys are required. The schemas are a closed set compiled into core:
`identity-v1` (identity report and release, notification) and
`identity-subagent-v1` (the same plus subagent start, stop and snapshot). Installing or
parsing a descriptor refuses an unknown schema id, an unknown handler id, and a
handler/schema pair the handler does not drive (`descriptor_invalid`). A
runtime without `[integration]`, such as Pi, has no schema, so its sessions
admit no hook report. The official Codex and Claude Code packages use
`identity-subagent-v1`, `hermes` uses `identity-v1`, and the shell, which can
host Codex and Claude Code in the foreground, uses `identity-subagent-v1`.

A schema declares the provider ids that may report, the admitted actions, the
native-reference kinds (`id`, `path`), the process-ancestry matcher, the
launch-process rule, whether a nested provider may become the active identity, the optional subagent claim
fields (`parent_id`, `agent_type`, and the `outcome` of a stop), and the
subagent sequence rule (a record's phase is the phase implied by the
highest-sequence claim admitted for its id: a start newer than a terminal
record reopens it, a stop newer than a running record ends it, a newer start
for a running record or a newer stop for a terminal one changes no phase but
advances the record's journaled sequence, and a claim not newer than that
sequence is ignored; the journaled sequence is capped at the worker's own
`CLOCK_MONOTONIC` reading when the claim arrives, so a far-future claim cannot
make the subagent's later genuine claims stale, while comparisons use the
claim's own sequence), and the subagent providers whose stop is
admitted only after a recorded start (`claude` in `identity-subagent-v1`, keyed
by the reporting provider, so it also covers a Claude in a shell session's
foreground). A schema that admits subagent
operations must declare the sequence rule; one without it admits no subagent
record on either side. The worker applies the declared matcher, launch-process
rule, field set and sequence rule when a hook reports, and the daemon applies
the matcher to the
imported identities, the provider set, the field set and the outcomes (a
failed or cancelled subagent needs the `outcome` field) to the imported
subagents, and the nested-provider rule to identities and releases. The
sequence order itself is checked by the worker only, because the inspect
snapshot carries no claim sequence for subagents. The worker orders identity
claims and releases per reported process (its pid and kernel start identity):
a claim or release must be newer than every claim and release the worker
accepted for that same process, and reports naming different processes are
never compared, so a report naming one process, even at `u64::MAX`, cannot
make a report naming another process stale. A hook reaches the worker as a
descendant of the agent it reports, so under `NamedForProvider` a descendant's
report naming an ancestor is that ancestor's report and is ordered with it;
under `FrozenRootEntrypoint` only the root itself may name the root. This
binding is not an authentication boundary: every process in the agent's PTY
subtree runs as the owner, so under `NamedForProvider` any process the launch
process starts can file the launch process's report, and on the worker path
only `FrozenRootEntrypoint` (`identity-v1`) binds the root's report to the
root's own kernel peer (see the trust boundary in the
[architecture](architecture.md)). Across
processes the latest accepted claim, in arrival order at the worker, is the
active identity, and the worker's imported active claim carries that process's
own sequence, which can be lower than the previous import's. The launch
process's native reference is ordered only by reports naming the launch
process. A release answered `ok: false` because its process is not the active
identity still orders that process's later-delivered claims. The worker
remembers the sequence of every live reporting process and never evicts one:
the mark of a process that exited (including one its parent has not reaped
yet), or whose pid now names another start identity, is pruned when room is
needed, and the PTY root and the launch process are always kept. When live
reporting processes besides those two fill the worker's bound (16), a report
from a further process is refused (`ok: false`, logged as
`worker.identity.rejected` with reason `identity_reporters_exhausted`) until
one of them exits. Before the launch identity exists, a launch agent's report
with a native reference is verified as a launch claim even then, and is kept
only when it establishes the launch identity, which pins its process; when the
verification is deferred, only its pending launch claim is kept, so the retry
can still establish it. The sequence contract of a worker
claim is therefore a value strictly increasing across every hook event that
reports the same process. Every hook event runs its own short-lived
interpreter, so the shipped Codex and Claude hooks read the host's system-wide
monotonic clock in nanoseconds (`CLOCK_MONOTONIC`) with Python's
`time.clock_gettime_ns(time.CLOCK_MONOTONIC)` rather than
`time.monotonic_ns()`, which is defined only within one process (Apple's
Python 3.9 on macOS counts it from interpreter start). The Hermes reporter
stamps its claims in its long-lived plugin process from `time.monotonic_ns()`,
kept strictly increasing by a counter. The launch-process rule decides which process may
set the launch identity, and the worker alone applies it; the daemon holds an
imported launch identity to the core ancestry matcher. `identity-subagent-v1`
uses `NamedForProvider` (the earliest process at or below the PTY root whose
executable is named for the provider or is a native-installer release image
`<provider>/versions/<release>`, or its declared hook-helper child), which
is also the core default for a session without a schema, never reached because
such a session admits no hook report. `identity-v1` uses `FrozenRootEntrypoint`:
only the PTY root, reporting in process, whose report's kernel peer must be the
root's PID and start time, so a report naming the root from any other process,
a descendant included, is refused; and only while its command line, executable
and entrypoint files match what the worker froze before spawning it, which the
worker rechecks for every later native reference of the root. The public
`session.report_native_id` fallback applies the same rule by attestation: it is refused for every report of a session on this schema (see the native-id report attestation rules below the schema section). Core keeps peer binding,
expiry, and self-target validation for every schema; a schema only chooses
which of the other rules applies and with which finite enumeration. The daemon
sends the
resolved schema id to the worker in `Initialize.hook_schema` (an additive
optional field of the private worker protocol), the worker journals it as
`hook_schema` and returns it in its inspect snapshot, and the daemon validates
the imported identity, release, and subagent state with it. A schema the
worker journaled always outranks the one the session's pinned runtime resolves
to. A worker that predates the field journals none; the daemon then validates
its state with the schema its session's pinned runtime resolves to (for a
package whose descriptor predates `hook_schema`, the schema its handler drives
when exactly one does). A report from an asset set newer than a session's
schema (a subagent hook reaching a session on `identity-v1`) is refused, and
the reports of an older asset set stay admitted by a newer schema.

The schema is also enforced on the public socket, the fallback the managed
hooks use when the worker refuses a report. `session.report_agent`,
`session.release_agent` and `session.report_native_id` answer `recorded: false`
/ `released: false` for a session whose runtime has no schema, whose schema
does not admit the action, or whose provider is not admitted, and
`notification.create` answers `hook_not_admitted` for a create that
names a session and a hooked provider (`shell`, `codex`, `claude`, `hermes`) on
such a session. The provider is judged by the name the record would be stored
and ranked under (control characters dropped, whitespace collapsed), and a
create whose raw provider only normalizes to a hooked provider (for example
`" codex"`) is refused with `hook_not_admitted` whatever the session's schema.
A session's own runtime is always an admitted provider for identity reports.

**Retryable admission.** A session without a served launch pin is admitted by
the schema its live worker journaled, and the public socket waits for that
worker up to `worker_connect_deadline` (10 s by default) before it refuses. The
managed reporters give the daemon socket only 0.5 s, so they would have left
long before the refusal. `notification.create`, `session.report_agent`,
`session.release_agent` and `session.report_native_id` therefore accept an
optional `retryable_admission: true`. With it, and only when the sole reason is
that the worker could not report its schema, the daemon waits just
`hook_admission_wait` (250 ms) and answers the daemon-class error
`hook_schema_unavailable` (recovery: the session's worker is reconnecting;
retry the report) on all four methods. A schema denial keeps today's answer
(`hook_not_admitted`, or `recorded: false` / `released: false`) even with the
field, so it stays final. Without the field nothing changes: the full wait and
the old answers. The field is additive and omitted when false. Because
`session.report_native_id` rejects unknown fields and both daemon generations
speak protocol 4, the protocol version cannot gate it: a daemon sets
`POHUNEK_HOOK_ADMISSION_RETRY=1` in the environment of every session it
launches (new, resumed and forked), and a reporter sends the field only when
that is set, so a session launched by an older daemon never sends it. A session
launched by this daemon keeps the variable if the daemon is later downgraded,
and the older daemon then rejects its native-id reports with `bad_request`;
downgrading under live sessions is outside the supported upgrade direction.
The managed reporters retry a refusal with `hook_schema_unavailable` (the
worker route first, then the daemon, with the same identifiers) until the
deadline of `POHUNEK_HOOK_RETRY_DEADLINE_MS` after the invocation started, and
treat every other answer as final. An agent hook that retries blocks its turn
for at most that deadline, and only while the session's worker is unreachable.
No other public wire shape changes.

**Native-id report attestation.** `session.report_native_id` is attested before any session state is read. The daemon takes the kernel peer of each local Unix connection once, when it is accepted (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED` with `LOCAL_PEERPID` on macOS, with the process start identity read at that moment); a remote TCP connection carries no peer. A report is admitted only when all of these hold: the peer is present; its user is the daemon's user; its process id and start identity are the ones the report names; and the peer is still that process when the report is decided, checked again immediately before the native reference is written. The report then goes through the ordinary checks, and only the session's PTY root can pass them. A report that fails the attestation, or any report for a session whose hook schema takes its launch identity only from the worker (`identity-v1`, `FrozenRootEntrypoint`, even from the root itself), returns the error `native_report_unattested` (class `runtime`), and its message names no request value. The recovery hint says to report the native reference through the worker hook socket from the launch process. A report that passes the attestation but fails a later check keeps the existing `recorded: false`. Each refusal writes one `warn` log record, `session.native_report.unattested`, with a `reason` of `no_peer`, `peer_uid`, `peer_not_root`, `peer_identity_changed` or `frozen_root_schema`; the record carries the session id and no request value. Hook processes are children of the agent, so the shipped Claude and Codex hooks' daemon fallback is refused, and the Hermes reporter's fallback is refused by the `identity-v1` rule; a native reference is recorded through the worker hook socket. No protocol version changes.

Rule for runtime packages without an integration handler: such a package gets
terminal launch and activity detection. It gets resume and fork only when its
agent CLI accepts a caller-chosen session id and the definition declares
`strategy = "assigned"`; with `hook` or `none` it is not resumable. A package
cannot obtain a reference through `session.report_native_id`, because that
method accepts only a report from the launch process itself; an `assigned`
runtime that declares an integration also takes reported references (see
below).

`assigned` takes, in addition to a supported `[resume]` with
`reference_kind = "id"` (core can generate an id, not a path):

```toml
[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{reference}"]   # passed at launch
assign_fork = true                               # optional; forks get one too

[native_reference.existence]                     # required, explicit
check = "file"                                   # or "none"
root_env = "PI_CODING_AGENT_DIR"                 # config home variable
root_home = ".pi/agent"                          # used when the variable is unset
dir = "sessions"                                 # directory below the root
file_name = "_{reference}.jsonl"                 # exactly one {reference}
name_match = "ends_with"                         # or "exact"
max_depth = 1                                    # directory levels below dir
store_env = "PI_CODING_AGENT_SESSION_DIR"        # optional; names the store itself
store_max_depth = 0                              # required with store_env
relocating_args = ["--session-dir"]              # optional; refused at launch
relocating_settings = [                          # optional; refused at launch
  { base = "root", path = "settings.json", key = "sessionDir" },
  { base = "cwd", path = ".pi/settings.json", key = "sessionDir" },
]
```

- `launch_args` follows the same whole-token `{reference}` rules and bounds as
  `[resume] args`. At launch the daemon generates a fresh hyphenated UUID (never
  taken from a client or user), appends the rendered `launch_args` after the
  runtime's fixed arguments and before the initial prompt argument, and stores
  the reference in the session and its recovery binding before the agent starts.
  Resume and fork use the declared `[resume]` / `[fork]` argv with that
  reference; the start flag is never repeated on resume.
- `assign_fork` (a boolean, default `false`, accepted only with `assigned` and
  `[fork] supported = true`, otherwise `descriptor_invalid`) gives every fork a
  reference of its own, for an agent that accepts `launch_args` together with
  its fork arguments. `session.fork` generates a fresh UUID for the fork and
  appends the rendered `launch_args` after the rendered `[fork]` args (Pi:
  `pi --fork <source> --session-id <fork>`); the fork holds that reference with
  provenance `assigned` from its creation on, so it resumes like a fresh launch
  and its own existence check guards its recovery. Without `assign_fork` a fork
  holds no reference until a validated report names one. The flag is frozen
  into the session's native launch spec, so a session keeps the fork behavior
  it was launched with.
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
- The optional relocation keys describe how the agent can move its store, in
  the agent's own order of precedence. When the launch environment sets the
  variable named by `store_env` (an upper-case name outside `POHUNEK_*`) to a
  non-empty value, that directory is the store: the scan starts there, skips
  `root_env`/`root_home`/`dir`, and descends `store_max_depth` levels (at most
  4; required exactly when `store_env` is set). `relocating_args` lists option
  names (`-` then ASCII letters, digits and `-`) that move the store; a launch
  whose program arguments contain one, as a token or as `option=value`, is
  refused. `relocating_settings` lists JSON settings files (`base` `root` for
  the config home or `cwd` for the launch directory, `path` a relative path of
  plain components) and a top-level `key`; while the `store_env` variable is
  unset, a file whose key holds a value the agent acts on (anything but
  `null`, `false`, `0` and `""`), or a path that is not a regular file that
  parses as JSON (opened without blocking, bounded to 1 MiB, a UTF-8 byte
  order mark is skipped), refuses the launch;
  an absent file, or a `root` setting below an unresolvable config home, sets
  nothing. Both lists hold one to 8 entries. A first launch (`session.new`) of a
  runtime with native recovery runs this refusal before anything is spawned,
  and `session.resume` and `session.fork` run it against the session's
  arguments and working directory before the existence check; a variable that
  is set to a relative path is refused the same way. The refusal is
  `runtime/agent_native_store_relocated`. At a first launch its `recover` hint
  names the `store_env` variable when one is declared (for a relocating
  argument, which wins over the variable, it says to remove the argument). At
  resume or fork the launch passed the same refusal, so something changed
  since (a setting or argument appeared, or the variable the launch had is
  gone after a daemon restart or an accepted profile edit), and the hint says
  to undo that change: remove the setting or argument, or restore the variable
  to the absolute path the session was launched with and accept the profile
  change. A crash of the check itself is
  `daemon/native_store_check_task_panicked`, and nothing is launched. The keys are frozen with the rest
  of the declaration, so a session launched before they were declared keeps
  searching only the declared store.
- A hook runtime may declare the existence check recovery runs on an id-kind
  reference under `[native_reference.existence]` (same shape as the `assigned`
  declaration; a path-kind hook reference cannot carry one). Before
  `session.resume` or `session.fork` launches, the declared check runs with the
  same rules as below. The official Claude Code package declares it: its hook reference is
  checked against a regular transcript file in the `projects` tree below the
  runtime's effective declared config home. An absent or unverifiable file
  returns `runtime/agent_native_reference_missing` without a new worker
  generation or fork child; a crash of the check returns
  `daemon/native_store_check_task_panicked`, also before anything launches.
- A fork starts a new conversation whose reference core cannot learn without
  an integration report, so the child holds no reference and is not resumable
  until its own launch process reports a validated target.
- A host profile on an `assigned` base inherits the assignment; one that
  restates `[resume]` is rejected with `invalid_profile`, and
  `resumable = false` switches recovery off (no reference is generated).
- A reported reference always supersedes an assigned one. An `assigned`
  runtime that also declares an `[integration]` handler and hook schema may
  report, and the report replaces the stored value and sets the provenance to
  `reported` in the session, its durable record and its resume binding, so a
  `/clear` or in-session resume inside the agent is followed instead of
  leaving a stale id. Two reports reach the daemon. `session.report_native_id`
  replaces the reference when it passes the caller attestation and the usual process-identity, expiry and
  sequence checks (a refused report leaves the assigned reference in place). The
  worker's launch claim replaces it even when the value is equal, which only
  confirms it, and the worker's journaled native reference replaces it: the
  worker keeps the latest reference reported by the verified launch process
  (below a wrapper process, the provider process it verified) with its hook
  sequence, independently of the active claim's lease and of a release, so a
  switch made while the daemon was down is still read when it returns. The
  reference must name the runtime itself, never a nested process, carry the
  declared reference kind and be newer than the last worker report accepted for
  the runtime generation. The journaled reference is trusted by its generation binding (the record's own worker instance and the verified launch process), never by whether that process or its wrapper is still alive; an exit, an explicit stop and a lost worker read the generation's final snapshot before they commit. A public report is ordered against the worker's journaled claim at acceptance: the daemon inspects the worker first, and the observed claim is older than the report whether or not it was imported yet (a worker that cannot be inspected makes the report retryable). A worker that crashed or is unreachable still hands over the reference of its own generation's journal when the session is classified, stopped or reconciled. A report of the launch process that arrives while the
  worker is still verifying it is kept and promoted with the claim, and a
  session whose runtime ended while the daemon was down imports the journaled
  reference too, when it belongs to the record's own worker instance and the
  verified launch process. A worker that journals no such reference is read
  through its launch and active claims instead, for as long as the active
  claim's lease lasts. Every replacement carries an ordering key, and a
  recovered session keeps the key of the reference it recovers. Sequences are compared only within one transport and one runtime generation
  (worker claims and the public reports of the shipped hook reporters are
  stamped from different clocks), so a public report is never stale because of a worker claim or the
  other way round, and a recovered or restarted generation starts fresh: its
  first launch claim supersedes the reference it was relaunched with. A report
  older than the stored one of its own transport, an expired or overlong
  claim, or a claim from another runtime never replaces a reference, and
  an assigned value never replaces a reported one: relaunches, daemon restarts
  and rebinding start from the stored reported reference. Resume and fork then
  launch with the reported reference. The existence check runs only for an
  `assigned` reference, so a reported one is recovered unchecked, as the agent
  vouched for it. A `hook` runtime uses the same generation, verified launch
  process, provider, frozen kind and sequence checks for the worker's journaled
  native reference, so an in-agent conversation switch can replace its first
  immutable launch claim. A package without an integration cannot send a report, so its
  reference stays `assigned`.
- A forked session of an assigned reference still shows the frozen
  `capabilities.resume = true` but holds no reference, so `session.resume`
  answers `not_resumable` for it.
- The values in the example are those of the repository's `pi` package
  (`runtime-packages/pi`), verified against a real Pi. Verify any other
  runtime's flag and on-disk layout before declaring them.

### Version probe

A runtime definition may declare a version probe in `[runtime]`:

```toml
version_probe = { parser = "semver-v1", args = ["--version"], min = "1.0.0", below = "2.0.0" }
```

`parser` names a compiled parser. `semver-v1` is data-driven: `args` (one to
four tokens) is the probe argv, and `[min, below)` is the supported release
range, so a package update can move the range without a daemon release. The
official packages declare `min` as their tested minimum (the verified release,
except Pi, which accepts from 1.0.0 and is verified on 1.4.0, with the 1.0.2 captures kept as the previous release, and Codex and Claude, whose
`min` is the previous verified release, with its captures kept in the
repository) and `below` as
the next major release (codex `[0.160.0, 1.0.0)`, claude
`[2.1.289, 3.0.0)`, hermes `[0.20.0, 1.0.0)`, pi `[1.0.0, 2.0.0)`), because hosts
update the agent CLIs independently of pohunek. The
declaration is rejected unless `args`, `min` and `below` are all present, `min`
is below `below`, and each bound is exactly `MAJOR.MINOR.PATCH` (for
`semver-revision-line-v1`, also `MAJOR.MINOR.PATCH-R<n>[.<m>]`). The daemon
compiles only the data-driven `semver-v1`, `semver-line-v1` and
`semver-revision-line-v1` parsers; a
descriptor that names any other parser id still parses, but its runtime reports
`supported: false` and a launch is refused with `agent_runtime_unsupported`.

`semver-line-v1` is the same data-driven parser for a runtime whose banner
wraps the release in fixed text. It additionally requires `line`, a literal
template `PREFIX{version}SUFFIX` with an optional trailing ` {annotation}`
(`{version}` exactly once; 1 to 128 printable ASCII bytes; no other braces; no
leading or trailing space; the prefix must not end in a digit or `.`, the
suffix must not start with a digit, `.`, `-` or `+`; at least one literal
character next to `{version}`). The official runtimes read as
`codex-cli {version}` (`codex-cli 0.160.0`), `{version} (Claude Code)`
(`2.1.289 (Claude Code)`) and `Hermes Agent v{version} {annotation}`
(`Hermes Agent v0.20.0 (2026.8.3)`). The trimmed first output line (at most
256 bytes) must match the whole template; later lines are ignored. The
template is literal text, not a pattern, and is matched without backtracking.
`line` is rejected unless the parser is `semver-line-v1` and is required by it;
an invalid template is rejected when the package is validated, naming
`runtime.version_probe.line`. Pre-release and build suffixes
(`0.160.0-rc.1`) never match: a pre-release cannot be ordered against
`[min, below)`, so such a runtime is unsupported.

`semver-revision-line-v1` (runtime feature `version-probe-revision-v1`, so the
descriptor lists it in `requires`) reads the same `line` templates but its
release token is `MAJOR.MINOR.PATCH-R<n>` or `MAJOR.MINOR.PATCH-R<n>.<m>`
(canonical digits, at most 64 bytes in all, and `n` and `m` below 2^32). The revision is a number, not a
pre-release: the order is `(MAJOR, MINOR, PATCH, n, m)` with an absent part as
0, so `1.4.13` equals `1.4.13-R0` and `1.4.13-R2` precedes `1.4.13-R2.1`, and
`min` and `below` accept both forms (`min = "1.4.13"` supports `1.4.13-R2`;
`1.4.12-R9` is below it). It requires `line` like `semver-line-v1`. The
existing parsers stay strict, and `AgentRuntime.version` stays an opaque string
that reports the release as printed (`"1.4.13-R2"`), read by `host.inspect` and
the launch gate through the same grammar.

Before `session.new`, `session.resume` and `session.fork`, and for the `host.inspect` runtime inventory
(`version`, `supported`), the daemon resolves the program once, runs it with the
declared argv, and parses the trimmed first line of its standard output as one
release (ASCII digits only, no leading zeros, no pre-release or build suffix;
bare for `semver-v1`, inside the `line` template for `semver-line-v1`).
Standard error is discarded. The probe runs with a cleared environment, a
private `HOME`, XDG and temporary directories and a private working directory,
a 2 second deadline and a 4 KiB output cap; only `PATH` is inherited from the
daemon, so an interpreter script such as `#!/usr/bin/env node` resolves the
interpreter from the same `PATH` that the default agent environment allowlist
forwards to the launched agent (an allowlist without `PATH` leaves the agent
without one). A program that does not resolve to an executable refuses the launch with
`agent_binary_missing` before any probe runs. Every other refusal is
`agent_runtime_unsupported`, and the exact probed executable is the one
launched. Its message tells the cases apart without echoing probe output: a
release outside the range names the parsed release and the declared
`[min, below)` (`reports version 1.0.0, outside its runtime's supported range
[0.160.0, 1.0.0)`); output that exits successfully but does not match the
grammar says the output does not match the runtime's declared format; a
failing or timed-out probe, or a parser id this daemon does not compile in,
keeps the generic message. The first two carry a `recover` hint. `pohunek
doctor` reports the same three cases in the profile and optional-runtime
checks.

The `host.inspect` inventory reads its probes through a daemon-wide cache. A
cache entry is keyed by the probed program's canonical path and file identity
(device, inode, size, modification and change time), the probe arguments and
the `PATH` the probe sees, and stores the raw output or the failure; the
supported range and the output grammar are applied on every read. An inventory
runs each distinct probe once, in parallel, and concurrent inventories share
that run. A program replaced, relinked or touched has a new identity and is
probed again at once; an unchanged program is probed again only after
`inventory.probe_cache_ttl_ms` (a failed probe after
`inventory.probe_failure_ttl_ms`), so an interpreter or dependency upgraded
under an unchanged script is noticed after at most that lifetime. The probe
spawns without `pre_exec`, so its cost does not grow with the daemon's memory;
its output is read from a pipe and a probe that writes more than the 4 KiB cap
is ended as failed. Launch-time validation (`session.new`, `session.resume`,
`session.fork`) never reads the cache.

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
  that predate the field load both flags as `false`. `session list` and
  `session inspect` report a flag as `false`, even though the frozen value is
  `true`, when the session's recovery resolution refuses it at the time of the
  request: the runtime it was launched from is not served, its package fails
  verification, or the serving runtime cannot honor the frozen launch spec.
  Refusals that depend on the session's current state (a live runtime) or on
  checks of one request (a changed profile, the native reference) come only
  from the operation itself. Host profiles declare the
  spec in `[resume]` (`reference_kind`, `args`, optional `fork_args`, accepted only on
  bases whose runtime declares a native fork, such as `claude`); see the
  agent-profiles concept page. The capability object itself is unchanged in
  this release: it stays strict, and the refusal reasons ride in
  `capability_refusals`.
- `capability_refusals`: optional `{resume?: string, fork?: string}` object on
  `session list` and `session inspect` responses, present only when the daemon
  cleared a frozen `true` capability. Each value is the error code
  `session.resume` or `session.fork` returns for that recovery resolution (for example
  `runtime_served_by_package`, `runtime_builtin_removed`,
  `runtime_not_installed`, `runtime_incompatible`, `agent_not_resumable` or
  `agent_fork_unsupported`), derived from the same recovery resolution that
  clears the flag, so for a refusal of that resolution the listing and the
  operation name the same reason. A frozen
  `false` capability carries no reason. Stored records, events and other
  results never carry the field. The object is open: clients ignore operations
  they do not know. Protocol 3 connections never receive it. Human
  `session list` output adds a `refused <id>: resume=<code> fork=<code>` line,
  and `session inspect` a `capability_refusals` row.
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
  runtime ends. A terminal child returns to `running` with a new `revision` when
  its provider starts it again (a resumed or self-woken Claude background
  agent): `finished_at_ms` is cleared, `started_at_ms` keeps the first run's
  time, and `updated_at_ms` marks the reopen. A delayed claim older than the
  latest one the worker admitted for a child never changes that child. A
  Claude `SubagentStop` without a
  recorded start (the prompt-suggestion helper Claude runs after every main
  turn) produces no item; a Codex one records a completed child. At the end of
  every Claude main-agent turn the rows of that Claude process are reconciled
  against Claude's own list of running background tasks. The list only settles
  rows it proves ended: a running row missing from this list whose newest
  listing by the same process falls within its run and is older than this
  list becomes `completed`, and any other missing running row becomes
  `completed` only when the list holds no task that can host an unlisted
  agent and the same process's newest list is older than this one and newer
  than the row (Claude builds a list before the hook reads its clock, so only
  an earlier list proves the agent had started). A list older than that
  newest evidence, delivered out of order, settles nothing from it. A listed row is left unchanged, whether running or terminal; a
  self-woken agent reopens through its own start. A completed row keeps its own
  claim sequence, so a resume start ordered before the list still reopens it,
  and a row with a newer claim than the list is never touched. A reconciled row
  has the same shape as any other; the reason is only in the worker log. A row
  whose Claude process never ends another main turn stays `running` until its
  own stop or the end of the runtime (`lost`). No prompt, result, transcript path,
  or raw hook payload is exposed. Subagent rows carry lifecycle fields only; the
  sanitized task label and shell program name appear in `background` instead.
- `background`: optional latest snapshot of the agent's background tasks, or
  absent or `null` when no snapshot is held: no agent has reported one, or the
  snapshot was cleared because the runtime ended, the reporting Claude process
  exited or released its session (except for `/clear`), it started or resumed a
  session, or another agent process started one. It holds a decimal-string
  `revision`, `observed_at_ms` (the worker's observation time, from which clients
  render the snapshot age), and `items`. Each item has `id`, `kind`, `status`,
  optional `agent_type` (subagent items), optional `label` (control and format
  characters removed, whitespace collapsed, bounded), optional `program` (shell
  items: the program name only, never its arguments or the full command line),
  `first_seen_ms`, and `last_seen_ms`. `kind` is one of `subagent`, `workflow`,
  `shell`, `monitor`, `mcp_task`, `teammate`, `dream`, `auto_mode_scan`,
  `memory_import`, `cloud_session`, or `other`; `other` carries the provider's
  task type in `provider_kind`. `status` is `running`, `pending`, or `other`.
  `first_seen_ms` is pohunek's first observation, not a start time: the task
  started at or before that instant. A subagent item shares its `id` with the
  matching `subagents` row, which stays the authority for subagent lifecycle.
  The snapshot is replaced wholesale on every change.
- `cwd`: current host-local working directory. It starts as the launch
  directory and can change while the session runs when procwatch observes the
  focus process in a new directory or the PTY emits an OSC 7 cwd hint.
- `cwd_source`: optional source of the current `cwd`: `launch`, `procwatch`, or
  `osc7`. `procwatch` is authoritative; `osc7` is an immediate hint that the
  next procwatch tick can overwrite if the focus process disagrees.
  A value a newer daemon defines reads as `unknown` on the Rust client and the
  CLI, as do the other display values below (`state_source`, `activity`, a
  subagent's `lifecycle` and `activity`); a background item's `kind` and
  `status` read as `other`. The daemon never emits `unknown`, rejects it in
  params and in its store, and the hidden variant is not in the TypeScript
  types.
- `pid`: root process id, or the observed external agent process id.
- `runtime`: optional durable runtime object. It is absent for observed external
  sessions and peers predating worker-backed sessions. `runtime_generation` is
  a canonical unsigned decimal JSON string, not a JSON number. `runtime.state` is
  `starting`, `live`, `reconnecting`, `terminal`, `lost`, `conflict`, or
  `incompatible`. A state a newer daemon defines reads as `unknown` on the Rust
  client and the CLI: the session may still own a live PTY, so `service uninstall`
  counts it as live (with `--stop-sessions` it waits until the stop timeout) and
  `session stop` and `session rm` refuse it locally with
  `session_runtime_state_unknown` before sending a request. The daemon never
  emits `unknown`, and its session store stays strict. `worker_id` identifies the
  PTY owner and `worker_instance_id`
  identifies the PTY generation. `started_at`, `last_connected_at`, `loss_reason`
  and `exit` are optional. `exit` (`RuntimeExit`) says how the service manager
  saw the worker end and is present only on a `lost` runtime whose failed
  worker job the daemon read before retiring it: `cause` is an open string
  (`oom_kill`, `signal`, `exit_code`, `core_dump`, `timeout`, `watchdog`,
  `resources`, `other`; a client treats any other value as unknown), `signal`
  and `code` are the optional signal number or exit code of the worker process
  (for `oom_kill` they describe how the worker process died, usually the stop
  signal, and the cause comes from the manager's verdict alone). It is absent
  after a host restart, on macOS (launchd keeps no verdict; tracked in #1343),
  for a worker started directly by the daemon, when the manager held no failed
  job, and when the read failed; it is kept while the runtime stays `lost` and
  cleared when the runtime becomes anything else (a stop the user requested
  ends the runtime `terminal`, without an exit). It is diagnostic: it does not
  change `state` or the proof of the loss, and an unproven loss records it too.
  Peers on protocol 3 never see it, and a 0.35 client ignores it. Daemon
  reconnection preserves both identities;
  explicit native recovery changes them. A `lost` runtime whose end the daemon
  observed (the worker's job ended and was retired, a host restart, or an
  answering worker whose child process is gone) belongs to a `failed` session with no `activity`
  and no `exit_code`; a `lost` runtime without that proof leaves the session's
  `state` as it was and has no `activity` (always the case for
  `runtime_lost_cleanup_unconfirmed`, whose processes may survive). Operations that need a running
  session (`session.input`, `session.resize`, attach) answer a lost runtime
  with `session_runtime_lost` and its recovery hint, whatever the session
  `state`. N-1 (0.35) clients read `failed` with `lost` as an ended session
  that cannot be stopped; `session.resume` keys on the runtime and still
  recovers it.
- `cols`, `rows`: current PTY size. External sessions have no PTY and report
  `0x0`.
- `state`: `starting`, `running`, `stopped`, `done`, or `failed`. `failed`
  also presents a runtime that was lost with an observed end (see `runtime`).
- `state_source`: `osc_title`, `osc_progress`, `screen`, `process`, or
  `report`. `osc_progress` is an OSC status report of the program: OSC 9
  progress or an OSC 7501 program-status record.
- `activity`: optional `working`, `blocked`, or `idle`. `idle` means the agent shows its
  prompt and is ready for input; a startup or confirmation dialog (folder trust,
  login, permission) reads `blocked`. Readiness for a first input is a separate
  per-runtime signal, the `[input] ready` gate of the runtime package, which
  `session.new` with `input` waits for when the input is written to the terminal
  (not passed as a prompt argument); `state = running` alone says neither.
- `native_session_id` / `native_session_path`: optional agent resume binding.
  These belong to the verified launch agent and follow its accepted conversation
  switches; nested active-agent reports cannot replace them. A fork reads the
  source's current target but starts without a recovery reference of its own.
  Hermes resumes only when this valid reference exists, as
  `hermes chat --resume <reference>`; it never infers an ambient Hermes
  session.
- `native_last_activity_at`: optional RFC3339 timestamp of the selected
  conversation's verified regular transcript file modification (the check the
  session's runtime descriptor declares, currently the official Claude Code
  package).
  `session.inspect`
  and `session.list` derive it on read without following symlinks. It is absent
  when no native file is verified and never falls back to `updated_at`.
  Human `session.inspect` labels target presence `native_binding`; the
  authoritative recovery decision is made by `session.resume` or `session.fork`.
  Human `session.list` shows the target and timestamp in a `recovery` detail
  line for every session with a native binding, using `<unknown>` when no file
  activity is verified.
- `project_id`, `project_label`, `repo`, `branch`, `worktree_path`: optional git
  and project context for the current `cwd`. A cwd change re-resolves this
  context. When a session leaves every known active worktree, `worktree_path` is
  cleared; `repo` and `branch` remain populated when git detection still finds a
  repository at the new cwd.
- `warnings`: non-fatal warnings, each `{kind, message, detail?}`. The Rust
  client of this build drops a warning whose `kind` it does not know, so a
  newer daemon can add kinds without breaking its decode; earlier Rust clients
  reject an unknown kind. A daemon of this build that loads a store record
  written by a newer daemon drops such a warning too and keeps the session;
  when it later rewrites that session's record, the dropped warning is not
  kept, so a later upgrade does not show it again. `kind` is
  `fetch`, `base_branch_fallback`, `setup_script` or `hook` for worktree setup,
  and `branch_ahead_of_remote` for a `branch_source: "remote"` session whose
  local branch is ahead of `origin` (`detail` is `local=<commit> remote=<commit>`).
  `base_branch_fallback` marks a requested base branch that was not used
  because the session's `branch` already existed locally at another commit and
  was checked out as it is, with `detail` naming both commits. A requested base
  is never replaced by the repository's default branch: a base that is not local
  and cannot be fetched fails the create with `invalid_base_branch` (it does not
  exist) or `base_branch_fetch_failed` (the fetch from `origin` failed; the
  message names git's failure). Worktree operations in one repository wait for
  each other for up to the git command timeout; finding the lock (a
  `git rev-parse --git-common-dir`) and the git command that follows each have
  their own timeout, so one operation can take up to about three times that
  bound (a fetch also runs a few unlocked git steps, each with its own
  timeout); a wait that runs out is reported the same way, with the text
  "waiting for another worktree operation on the same repository" in the `fetch`
  warning's `detail` or in the `base_branch_fetch_failed` message; the ordering
  covers one daemon only. `kind` is `native_recovery` for a session whose stored
  native recovery record could not be completed after an upgrade: it cannot
  resume or fork, `message` says to start a new session and resume the native
  conversation, and `detail` carries the stored native reference. `hook` also
  marks a launch (new, resume or fork) whose agent's installed Claude or Codex
  hook assets differ from the ones the
  daemon embeds (an older version marker or changed content): `message` names the
  `pohunek integration install --agent <agent>` command (with `--profile` for a
  host profile's home) and `detail` carries the status warnings; the session
  still starts, and an agent whose hooks are not installed carries no such
  warning. For a failing `pre-create` or `post-create` lifecycle hook, the
  `session.new` result's `detail` ends with `; stderr tail:` and the last
  `hooks.stderr_tail_bytes` bytes (`service.toml`, default 4096, `0` disables)
  of the hook's stderr: lossy UTF-8 with terminal escape sequences and control
  characters other than line feed and tab removed, prefixed by
  `[N earlier bytes omitted]` when cut. Only that result carries the tail;
  `session.list`, `session.inspect` and the logs keep the detail without it. Hook
  authors must not print secrets to stderr.
- `output_force_closed`: optional boolean, omitted unless `true`. A terminal
  session (`stopped`, `done` or `failed`) whose PTY output was force-closed
  carries `true`: a process outside the session's process group kept the
  terminal open past the stop deadline, or past the drain window after the
  root exited on its own, so output written after the close is not retained,
  while the exit is exact. The runtime ends this way only after the worker
  swept the runtime's marked processes, as described for `session.stop`. It is
  additive in protocol 4; the protocol 3 adapter drops it.
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
resolves is rejected with `runtime/runtime_not_installed`. A session's
`active_agent_base` of another runtime than the session's own (an agent a shell
session hosts in its foreground) is only checked against the grammar: mutations
act on the session's own runtime, so a shell session stays interactive while
the hosted agent's package is missing or uninstalled. Unknown values never
silently become a supported launch, resume, or fork runtime. A session or
recovery record whose runtime is not installed is kept inert: it stays listed
with its stored recovery binding, refuses resume, fork, input and every other
mutation with `runtime_not_installed` (or `agent_kind_unsupported` for a
historical label), and resumes again once the runtime is installed. The
exception is a session launched from the built-in runtime of an official alias
this daemon does not compile in (the built-in Codex, Claude Code or Hermes of
an earlier release): it is refused with `runtime/runtime_builtin_removed` while
no package serves the alias. The official package then adopts the session and
resume and fork run through it when the recovery contract is honored; otherwise
they are refused with `runtime/runtime_served_by_package`.
`session.stop` and `session.remove` (also
`session.remove_accepting_unconfirmed`) are the exceptions: they only release
what the session owns and need no runtime definition, so the owner can end the
session's worker and delete the session while its runtime is missing. Removal
keeps its supervision rules (a worker not proven gone refuses), and an external
session stays refused with `session_external_read_only`. The
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

Frozen profile revision. A session launched from a host profile freezes the
profile's keyed revision (the same MAC a relay approval uses) into its resume
binding when it is created, so the binding names which profile the session ran
under without storing its `[env]`. `session.resume` and `session.fork` resolve
the profile once and use that single resolution for the version probe, the
native-reference check, the launch and the relaunched session's snapshot. They
relaunch only while the profile's current revision equals the frozen one:

| Frozen revision | Current resolution | Outcome |
|---|---|---|
| present, equal | profile | relaunch under the resolved profile |
| present, different | profile | `agent_profile_changed` |
| present | no profile, or the bare runtime | `agent_profile_missing` |
| absent (legacy binding) | profile | `agent_profile_changed` |
| absent | profile that does not resolve | `agent_profile_missing` |
| absent | bare runtime | relaunch without profile env |

The owner's decision `accept_profile_change` relaunches under the current
profile's env and detection manifest and freezes its revision, so a following
resume without the decision succeeds. The launch shape frozen at creation
(program, arguments, native resume spec, input rules, runtime pin) is not
re-derived from the edited profile. A missing profile is refused even with the
decision, because there is no current profile to relaunch under. The decision is
accepted on the local control socket only (`agent_profile_change_local_only`
otherwise); the control socket does not distinguish the operator from an agent
running inside a session on the same host. `plugin profile migrate` and
`package.bind_profile` rewrite the profile text. An adopted built-in session keeps
its frozen revision over the migration pin (the `package` and `digest` keys), so
it resumes without the decision; any other rewrite of the text needs the decision
once for the existing sessions of that profile. The optional `profile_revision` field of a
resume binding (store schema 3 and later; the current store schema is 12) is
absent for sessions without a profile and for bindings written by schema 2.

A host profile over a package-served base carries the top-level keys
`package` (package id) and `digest` (`sha256:` plus 64 hex characters), both or
neither; a profile over a built-in base carries neither. The profile resolves
from exactly that digest, so `package.select` and an update of the package do
not change what it launches and do not change its revision, while a migration
that changes the digest does. A package-served base without the keys is
`runtime/invalid_profile` with the migration command
`pohunek plugin profile migrate <name>` in its message. A fresh launch through
a pinned profile still requires its package to be enabled and verified
(`runtime/runtime_not_installed` or `runtime/runtime_incompatible`); a session
already pinned to the digest resumes. A digest pinned by a host profile counts
as referenced for `package.uninstall`, `package.list` and `package.doctor`, and
`host.inspect` reports a pinned profile `available: false` while its package is
disabled, uninstalled or fails verification.

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
output offsets, terminal watermarks, background revisions, process-start
identities, and report sequences are canonical unsigned decimal strings. They remain exact beyond
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

A manifest that declares `[program_status]` (see "Declaring program status" in
the runtime-packages guide) takes part in activity detection, but `session.detection` neither lists
`program_status` in `supported_regions` nor returns a preview for it:
`DetectionRegionKind` is a strict enum on the wire, so a client that predates
the kind would fail to decode the result. Message, title and progress texts of
a record are never stored or exposed.

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
`after_updated_at` is RFC 3339 and is compared as an instant with the session's
`updated_at`, so the offset and the length of the fractional seconds do not
matter. Echo `SessionInfo.updated_at` verbatim: a cursor re-rendered at a lower
precision (for example milliseconds) is earlier than the stored stamp, and the
wait returns `session_updated` at once. Present `states` and `activities`
arrays cannot be empty:

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
globally and 8 per session. When the client closes the connection (EOF on the
read side) while a `session.wait`, a `session.output` with `wait_ms`, or a
`session.wait_turn` is in flight, the daemon cancels that read-only wait,
releases its slots, and closes the connection without a reply; closing only the write side counts as closing.
Other methods, including `session.input` with `wait`, run to completion. Every
accepted overlay (TCP) connection has TCP keepalive set from the `service.toml`
`[waits]` keys `keepalive_idle_ms`, `keepalive_interval_ms` and
`keepalive_count` (30 s, 10 s and 3 by default, Linux and macOS): a peer that
vanishes without closing the connection fails its read and releases the slot
of an in-flight wait within `idle + interval x count` (60 s by default)
instead of at the wait's deadline. The Unix socket is unchanged. The timeout
stays the release bound for a client that pipelines another request line
behind the wait: that line is answered in order after the wait's reply, and
the read side is not watched again until then.

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
`session_waiter_limit_reached`, `worker_feature_unavailable`, and, for a bounded
wait that daemon shutdown ends, the daemon-class `daemon_shutting_down`. Restart
from a fresh screen/tail after runtime change. A worker on the immediately preceding
private protocol remains usable for existing lifecycle and attach operations,
but observation returns `worker_feature_unavailable`.

When the daemon stops, a control or remote connection answers the request lines
it has already read, and any line that arrives after the signal, with the
daemon-class `daemon_shutting_down` (stamped with the connection's negotiated
version) and then closes; the line is not dispatched. A watched wait in flight
gets the same reply, an event subscription or attach stream ends as a normal
close, and a mutating request already running completes and is answered before
the connection closes. A client that cannot read the code, such as one on an
older protocol, sees an ordinary daemon error followed by the end of the
connection.

### Turn Waits

`session.wait_turn` blocks until one of the targeted sessions records a turn
event newer than a cursor, or until the timeout elapses. It is the primitive for
a manager agent that starts sessions and follows them: chain waits with the
returned cursor and branch on the reason. The CLI form is `pohunek session wait
<target>... [--until <kinds>] [--after <cursor>] [--timeout <duration>] --json`
(see `docs/cli.md`).

```json
{
  "session_ids": ["s-41", "s-42"],
  "until": ["turn_completed", "agent_blocked", "approval_required", "exited"],
  "after": "t1.17.1791800000000000000.d-4242-1791799000000000000-0",
  "timeout_ms": 300000
}
```

| Param | Type | Notes |
|---|---|---|
| `session_ids` | `SessionId[]` | Required, 1 to `MAX_SESSION_WAIT_TURN_TARGETS` (32), no repeats. Every target must exist when the call starts, or it fails with `session_not_found` before taking a waiter slot. |
| `until` | `SessionTurnEventKind[]` | Required, non-empty, no repeats: `turn_completed`, `agent_blocked`, `approval_required`, `exited`. |
| `after` | opaque string, optional | The `cursor` of an earlier result; 1 to `MAX_SESSION_TURN_CURSOR_BYTES` (256) printable ASCII bytes. |
| `timeout_ms` | `u32`, optional | Non-zero. Omitted means the daemon default (5 minutes unless `service.toml` `[waits]` `turn_default_ms` sets another). Above the daemon ceiling (`turn_max_ms`, 30 minutes by default) the call fails with `session_wait_limit_exceeded`; it is never clamped. |

The wait is edge-triggered: it completes only with an event recorded after
`after`, or after the call started when `after` is omitted, never with the
current state. A turn event is recorded when its debounce window ends (see
"Event sources"), so a turn whose event was recorded before the call is not
returned, while a stop reported shortly before the call whose window ends
during the call is. An event recorded between two chained waits is returned by
the second one.
With several targets the oldest matching event wins and names its session. The
call sends no heartbeat frames; the result is the only response line.

```json
{
  "reason": "event",
  "event": {
    "session_id": "s-42",
    "kind": "turn_completed",
    "at": "2026-10-12T10:05:00.123456789Z"
  },
  "cursor": "t1.18.1791800700000000000.d-4242-1791799000000000000-0",
  "progress": [
    {
      "session_id": "s-41",
      "state": "running",
      "activity": "working",
      "last_output_at": "2026-10-12T10:04:59.5Z",
      "output_advanced": true,
      "turn_started_at": "2026-10-12T10:01:12Z"
    },
    {
      "session_id": "s-42",
      "state": "running",
      "activity": "idle",
      "last_output_at": "2026-10-12T10:04:58Z",
      "output_advanced": true,
      "turn_started_at": "2026-10-12T10:03:40Z"
    }
  ]
}
```

| Result field | Type | Notes |
|---|---|---|
| `reason` | `SessionWaitTurnReason` | `event`, `timeout`, `cursor_reset`, or `targets_ended`. |
| `event` | `SessionTurnEvent`, optional | Present exactly when `reason` is `event`: `session_id`, `kind`, `at` (RFC 3339 time the daemon recorded the event, after any debounce), and `notification_id` when the notification policy kept an inbox record for the same event. |
| `cursor` | opaque string | Pass it as `after` to the next wait. |
| `progress` | `SessionTurnProgress[]` | One entry per target, in request order, in every result. |

Each progress entry carries `state`, `activity` (omitted when unknown),
`last_output_at` (the last terminal output this daemon observed, omitted when
none), `output_advanced` (output arrived after the cursor was issued, or after
the call started without a cursor), and `turn_started_at` (the last transition
into `working` this daemon observed, omitted when none; after a resumed turn it
can be later than the `turn_started_at` of the turn's notification). A timeout with
`output_advanced: true` and `activity: "working"` reads as "still working, wait
again"; a long-quiet `last_output_at` reads as "stuck or waiting on an empty
prompt, investigate".

Reasons:

- `event`: a selected event newer than the cursor was recorded.
- `timeout`: the duration elapsed without one. The cursor covers every event
  recorded so far, so the next wait does not see unselected events again.
- `cursor_reset`: the cursor is not one this daemon instance can place: it was
  issued by another daemon instance (for example before a restart), is older
  than the retained journal window, or is not a cursor this daemon issued. The
  call then returns at once with a fresh cursor at the current end and
  progress. A wait already in progress also resets when events after its
  position are evicted from the journal while it waits.
  Events between the old cursor and the reset are not reported; never treat a
  reset as "no event happened".
- `targets_ended`: every target is in a terminal state (`stopped`, `done`,
  `failed`) and no selected event is newer than the cursor. It returns at once,
  because no event can follow without an external resume.

The cursor is opaque. Store it, pass it back, and never parse or construct
one; only the issuing daemon instance can place it. Turn events live in a
bounded in-memory journal (1,024 events by default) and do not survive a
daemon restart.

Event sources:

- `turn_completed`, `agent_blocked`, and `approval_required` come from two
  trusted producers only: provider hook reports through `notification.create`
  with the `turn:<session_id>` or `attention:<session_id>` dedupe key, from a
  provider with a hook schema (Claude, Codex, Hermes) whose report the
  session's runtime admits, for a session this daemon holds (the worker relays
  its hook socket reports the same way); and the daemon projector's
  `agent_blocked` for a detected blocked activity and its `turn_completed` for
  a session whose activity goes from `working` to `idle`. A create from any other
  producer, or naming a session the daemon does not hold, is handled by the
  inbox only and records no turn event. Trusted signals are recorded whether or
  not the notification policy keeps an inbox record. With the default policy,
  `turn_completed` is disabled for the inbox, so `notification.create` still
  answers `notification_kind_disabled` and no record is listed, yet the turn
  event is recorded and completes waits.
- They follow the attention debounce: an event is recorded when
  `attention_debounce_secs` (5 seconds by default) has passed since the last
  signal for the same session and kind, so it arrives no earlier than that
  window after the agent stops, and later when signals repeat. An
  `agent_blocked` or `approval_required` is dropped when the session resumes
  work or ends inside the window. A `turn_completed` report that names no turn
  is dropped when the session is `working` when its window ends: that is the
  stale stop of an earlier turn, and the running turn reports its own stop. A
  report of a turn the daemon runs is held until that turn is over. A turn that starts
  and stops within the window (a short turn) is recorded once, after its stop.
- A turn ends, for the projector, when activity goes `working` to `idle`; a
  turn that ends `blocked`, an idle that no `working` preceded (start-up, an
  empty prompt) and a session whose runtime was replaced mid-turn report no
  `turn_completed`. A `working` the daemon infers from terminal output opens a
  turn only while the session's current runtime holds input evidence:
  `session.input`, a line submitted by an attached client, or an initial
  prompt (including one in the launch arguments) arms it, and an agent report
  does not. The turn that opens takes the evidence; if that turn is abandoned
  (its agent or process changed, it ended `blocked`) the evidence goes back
  and the next turn takes it, with the abandoned turn's start. The start-up
  banner, or the repaint of a resumed or re-adopted session, is no turn: a
  resume, replacement or re-adoption starts without any evidence. A report
  of `working` opens its turn with or without evidence. Output without
  evidence still continues a turn that is settling or waiting for approval. The report is held for the debounce window, so an idle that
  lasts less than `attention_debounce_secs` before the session works again
  reports nothing. A hook `Stop` and the projector's idle edge of one turn are
  one inbox record; a hook report that arrives after its turn was reported, within
  twice `attention_debounce_secs` (at least 2 seconds) of the record reaching the
  inbox, adds no second record, though a waiting `session.wait_turn` may see one more turn
  event. A daemon restart during a turn, or during the window, can lose that
  turn's report. A runtime without managed hooks (a shell, Hermes without its
  plugin) now completes `until: ["turn_completed"]` waits too.
- `exited` is recorded when a session the daemon holds moves from a running
  state into a terminal lifecycle state, at the moment the terminal state
  becomes visible; a wait that sees every target terminal therefore already
  finds their exits. A session that ended before the daemon started does not
  record one; a wait on it returns `targets_ended`.

Errors: `bad_request` (invalid params), `session_not_found` (an unknown target
at the start, or a target removed during the wait),
`session_wait_limit_exceeded` (`timeout_ms` above the ceiling),
`session_waiter_limit_reached` (the turn waiter pool is full), and
`daemon_shutting_down` (daemon shutdown started during the wait). The turn
waiter pool is separate from the `session.wait` pool: a call takes one global
slot and one slot on every target, all or none, and releases them when the
call returns or when the client closes the connection during the wait (no
reply is sent then). A client that vanishes without closing an overlay
connection frees its slot within the connection's keepalive bound, `idle +
interval x count` of the `service.toml` `[waits]` `keepalive_*` keys (60 s by
default); the timeout stays the release bound for a client that pipelines
another request line behind the wait. `session.wait` is a
separate method with its own 8,000 ms limit.

A protocol 3 connection does not define the method: the daemon answers
`method_not_found`, and a current SDK client fails with
`daemon/daemon_protocol_too_old` before sending it. A protocol 4 daemon of an
earlier release answers `daemon/method_not_found`.

### Idle Stop

A session stops by itself when it opts in and its agent stays `idle`. The
opt-in is session metadata, the first key the daemon interprets (`pohunek.` is
reserved for such keys; other `pohunek.*` keys and every key outside it are
opaque):

| Key | Value | Meaning |
| --- | --- | --- |
| `pohunek.idle_stop_after_secs` | `default` | Stop after the host default of `session.idle_stop.get`; a session is never stopped while no default is set. |
| | decimal seconds, at least `min_after_secs` | Stop after this idle time, whatever the host default. |

A session without the key is never stopped for idleness. `session.new` and
`session.set_metadata` refuse any other value (digits only: no sign, space,
exponent or unit) with `daemon/invalid_idle_stop_after`, before anything
starts; a refused edit leaves the previous metadata. Only a value being set is
checked, so a value an older daemon stored unvalidated never blocks an edit of
another key; such a session is not opted in, and the daemon logs a WARN once.

The idle clock restarts whenever the session's activity value changes
(`idle -> working -> idle`, `blocked -> idle`); a repeated `idle` report does
not move it. It lives in memory only: after a daemon restart it starts when the
session is adopted, so a restart can delay a stop and never advance one. The
daemon stops the session through the ordinary stop transaction when all of
these hold at the deadline, re-checked under the lock that marks the session
as stopping: the session is `running` on a live worker, its activity is
`idle`, no `agent_blocked` or `approval_required` notification for it is
`unread` or `read`, and no `session.input`, `session.wait` or
`session.wait_turn` call waits on it. While the notification store is
unavailable (`notifications_unavailable`) its attention records are unknown, so
every session counts as holding one and none is stopped until the daemon runs
over a usable store. A held session is looked at again after
a short interval. The worktree is kept and the `session-stop` hook fires.
The idle clock and the reported activity are memory only: after a daemon restart
a session is armed again when it reports `idle` again, and its limit counts from
that report.

The stop is recorded where an operator can find it, without a protocol field:
an INFO log line `session.idle_stop.stopped` (session id, idle seconds, whether
the limit came from the session or the host default) and the `session_stopped`
event in the event log. When the notification policy keeps `system` records, a
`system` notification for the session carries `reason=idle_stop` and
`summary=idle_secs=<n>`. The default policy has `system` disabled, and the
daemon honors that choice: the stop then leaves no notification.

The host default is a record of its own, `<data_dir>/session-idle-stop.json`
(owner-private, written atomically, `{}` when no default is set; a corrupt or
unsafe record fails daemon startup like `session-policy.json`). It is not part
of `SessionRetentionPolicy`, so a client of the previous release reads and
rewrites the retention policy exactly as before while a default is set.

A protocol 3 connection does not define the methods (`method_not_found`; a
current client fails with `daemon/daemon_protocol_too_old` before sending). A
protocol 4 daemon of an earlier release answers `daemon/method_not_found`, and
`pohunek session policy get` then reports `idle_stop_after: not supported by
this daemon`; such a daemon treats the metadata key as plain metadata, so the
session is simply never stopped for idleness.

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
- `POHUNEK_HOOK_TRACE_PATH` (absolute path of the hook failure trace,
  `pohunek-hooks.jsonl` in the pohunek log directory; see "Hook failure trace"
  under Provider Notification Hooks)
- `POHUNEK_HOOK_TRACE_MAX_BYTES` (size in bytes at which the state hooks rotate
  that trace)
- `POHUNEK_HOOK_ADMISSION_RETRY` (`1` when the daemon answers a hook report that
  sends `retryable_admission` with `hook_schema_unavailable`; reporters send the
  field only when it is set), with `POHUNEK_HOOK_RETRY_DEADLINE_MS` (how long a
  reporter keeps retrying, counted from the start of the invocation: the hook
  timeout minus a margin), `POHUNEK_HOOK_RETRY_BACKOFF_MS` (first pause) and
  `POHUNEK_HOOK_RETRY_BACKOFF_MAX_MS` (longest pause; the pause doubles). A
  reporter without all four values sends and retries nothing.
- `POHUNEK_BACKGROUND_LABEL_MAX_BYTES` (byte limit of a background task label
  the Claude `Stop` hook forwards; a missing or invalid value forwards no label)
- `POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES` (byte limit of a background shell's
  program name; a missing or invalid value forwards no program)

A managed child does not inherit the daemon's or the worker's process
environment. Its environment is built from an empty base: the allowlisted
variables the daemon passes from its own environment (the `[environment]
allowlist` of `service.toml`, by default `PATH`, `HOME`, `USER`, `LOGNAME`,
`SHELL`, `LANG`, `LC_*`, `TMPDIR`, `SSH_AUTH_SOCK`, `DISPLAY`,
`WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, and `XDG_*`), `TERM`, the agent
profile's environment, and the reserved values above. `TMPDIR` names the
session's private temporary directory,
`<cache_dir>/session-tmp/<store key>/<session id>` below the daemon's cache
directory (`$XDG_CACHE_HOME/pohunek` or the platform cache path), on Linux and
macOS alike. The store key is the first 16 hex digits of the SHA-256 of the
metadata store's canonical path (its directory resolved through
`realpath`, then `metadata.jsonl`), so daemons with different stores that share
a cache directory never share a root. It replaces a `TMPDIR` the allowlist
forwards, so concurrent sessions never share temporary names; a profile that
sets `TMPDIR` in its `[env]` keeps its own value and gets no private directory.
A profile over an agent whose manifest declares `[program_status]` can switch the
agent's program status off with the agent's own variable (`PI_PROGRAM_STATUS=0`
for Pi, `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` for Claude); detection is then
screen-only.
The directory and the directories above it up to `session-tmp` have mode 0700 and must
be directories of the daemon's user reached without a symlink; a launch whose
directory fails that check fails with runtime-class `session_tmpdir_unsafe`,
one whose directory cannot be created, or whose path is not valid UTF-8 and so
cannot be exported, with `session_tmpdir_failed`, before
any worker starts. A stop keeps the directory and a resume of the same session
reuses it (recreating it when it is gone); a fork gets its own. Every removal
(`session.remove`, retention, a rolled-back create) deletes it with everything
an agent left in it; a delete that fails is logged with its path and leaves
the removal itself successful. Daemon startup deletes every entry of its own
store's root that no startup evidence names: a session record, a resume
binding, a store line the daemon rejected that names a session, a worker
runtime slot or a worker's claimed session (also of an orphaned worker that
keeps running), a service-manager job of the session, or a worker journal.
That is what a crash, a failed delete, or a reset store leaves behind. It
deletes nothing, and logs why, when worker discovery, the service manager's
job listing (which must list every job it holds), or the journal scan fails
(WARN), or when a rejected store line names no session (INFO). An entry it cannot delete, such as a symlink, is logged and
kept, and startup continues. Other stores' roots are never entered. Ambient
`POHUNEK_*`
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
the job definition (install, an upgrade to a different version, or
`pohunek service refresh-path`); upgrades reuse the recorded list, so a manual
edit takes effect only at the next version-changing upgrade;
`pohunek service refresh-path` re-records the path and replaces the daemon job
with it (below). This key
raised the `service.toml` `schema_version` to 2, the `[input]` table added
for #496 raised it to 3, and the `[waits]` table added for #858 (`turn_default_ms`
and `turn_max_ms`, the default timeout and the ceiling of `session.wait_turn`,
each in `5000..=3600000` with the default at most the ceiling; fresh installs
and migrations write 300000 and 1800000) raised it to 4: the current release
refuses to serve a schema-2 to schema-6 file to the daemon, reads it
only at the `pohunek service upgrade` boundary, and upgrades it as a migration
that assigns the defaults of the missing keys and tables and keeps every
recorded value. The input write stall bound (`[input] write_stall_timeout_ms`, 3000 ms on fresh
installs and migrations, `1..=600000`) raised it to 5; a schema-4 file is
migrated the same way, with only the stall bound assigned. The `[hooks]` table
(`stderr_tail_bytes`, the bytes of a failing worktree-create hook's stderr kept
for the `session.new` warning, `0..=32768` with 0 disabling the capture, 4096
on fresh installs and migrations) raised it to 6; a schema-5 file is migrated
the same way, with only the hooks default assigned. The `[events]` table
(`max_file_bytes`, `max_files` and `max_age_days`, the size, count and age
bounds of the event journal; 33554432, 8 and 14 on fresh installs and
migrations) raised it to 7: the table is required, so under the rule below it
is a breaking change and not an optional additive key; a schema-6 file is
migrated the same way, with only the events defaults assigned. The optional
`[waits]` keys `keepalive_idle_ms` and `keepalive_interval_ms` (whole seconds
written in milliseconds, `1000..=600000`, 30000 and 10000 by default) and
`keepalive_count` (`1..=10`, 3 by default) set the TCP keepalive of every
overlay connection the daemon accepts (#862); they are additive optional keys,
so they leave `schema_version` at 7 and the writer omits each while it equals
its default. The optional `[deadlines]` key `registry_shutdown_ms` (`1..=600000`,
5000 by default) bounds how long the daemon's session registry waits for its
background tasks and in-flight store writes at shutdown; it is additive, so it
leaves `schema_version` at 7 and the writer omits it while it equals its
default. The optional `[deadlines]` keys `create_drain_ms` (10000),
`driver_shutdown_ms` (5000) and `event_log_flush_ms` (5000), all `1..=600000`,
bound the other daemon shutdown phases the same way (see the shutdown tree in
the architecture). The four phase bounds plus a 2000 ms margin should not exceed
`daemon_exit_timeout_ms`. A file that exceeds it stays valid: `pohunekd` logs a
warning at start and shortens the phase bounds in proportion so the service
manager does not kill it before its event log flushed, and the
`service_config_shutdown_budget` check of `pohunek doctor` is `warn` with the
configured exit timeout and the least one that fits. The defaults fit the
default exit timeout of 30000 ms. Of the margin, 500 ms is the bound on dropping the async runtime
after the last phase (a blocking write that never returns is left behind there, not waited for). The optional `[inventory]` table sets the version-probe cache behind
`host.inspect`: `probe_cache_ttl_ms` and `probe_failure_ttl_ms` (`1..=600000`,
120000 and 10000 by default), `probe_cache_max_entries` (`1..=4096`, 256) and
`probe_parallelism` (`1..=256`, 32, the most probes one inventory runs at
once); it is additive, so it leaves `schema_version` at 7 and the writer omits
the table while every key equals its default. The `[hooks]` table also takes the optional `required` key, a list
of the hook events whose failure ends `session.new` (only `"post-create"`, #670);
it is additive, so it leaves `schema_version` at 7, the writer omits it while it
is empty, and a previous release ignores the key and does not enforce it.
`service.toml` tolerates keys this version does not know (#1401). The
`schema_version` changes only for a breaking change: a removed or renamed key, a
changed meaning or unit, or validation that rejects a file that was valid
before. An additive optional key with a documented default does not change it,
and the writer omits such a key while it equals the default, so the previous
release still reads a file this release rewrote. A schema-7 file with an
unknown key or table loads; the key is ignored, and the daemon logs one
`service_config.unknown_key` warning per key at start (key path, line and file,
never the value; a worker generation logs it at debug only). `pohunek service
status`, the `warnings` of `service check` and of the success report of
`service upgrade`, and the `service_config_unknown_keys` check of `pohunek
doctor` (`warn` listing the key paths and lines with a "did you mean" hint, `ok`
when there are none) report them. The unknown keys of the tolerant tables and
sections are kept in place when the installer rewrites the file (an upgrade, a
reinstall or a rollback), and the upgrade says so in the same warning. An
unknown key in `[namespace]` or `[environment]` stays a hard error naming the
key, because an older binary must not skip a setting that moves a root or
narrows the environment; a new setting there or among the top-level identity
keys bumps the schema. A missing required key stays an error, with a "did you
mean" hint when an unknown key of the same table resembles it, and an invalid
value of a known key fails whatever unknown keys surround it. The schema 2 to 6
upgrade sources are read in their exact shapes and still refuse an unknown key.
The status report carries the same additive `warnings` list.
`service check` and the success reports of `pohunek service upgrade` name that
migration in an additive `config_schema_migration` field (the source schema,
`2`, `3`, `4`, `5` or `6`, or `null` when no migration runs); the check runs it read-only. The migration stops the daemon before it rewrites
`service.toml`, carries the exact pre-upgrade bytes in an owner-private backup
digest-verified against its transaction record, and restores those bytes on
rollback; a resume or rollback that cannot prove the exact pre-upgrade bytes
fails with `service_config_backup_invalid` and changes nothing (see
[update after release](knowledge/runbooks/update-after-release.md)). A version
1 file still has no migration: it is refused with a message to uninstall with
the pohunek that wrote it and install again. `pohunek service install --json`
reports the
outcome in an additive `search_path` object: `source` (`login_shell`,
`fallback`, `recorded`, `unmanaged`), `entries`, `shell_used`,
`shell_defaulted`, `login_shell_failure` (the rendered typed reason when the
fallback list was used), and `dropped` (`[{path, reason}]`, directories refused
as untrusted); the human output prints a `warning:` line for a failed login
shell and for each dropped directory. `pohunek service refresh-path --json`
runs the same discovery against an existing installation and reports `dry_run`,
`changed`, `applied` (the refresh took effect: `service.toml` records the path
and the daemon job runs with it), `recorded` and `discovered` (the directories
in search order; for a resumed refresh, the ones the interrupted run
journaled), `added`, `removed`, `shell_used`, `shell_defaulted`, `dropped` (as
above), `daemon_restart` (`restarted`, `would_restart` for a dry run of a
change, or `none`), `daemon_started` (the job was registered but not running),
`workers_kept` (worker jobs left running, `null` when they could not be
listed), `resumed` (an interrupted refresh was resumed), `rolled_back` (an
interrupted refresh that was rolled back instead; the run then stops), `preflight`
(the new daemon's adoption verdict, as `service upgrade --json` reports it),
`accepted_runtime_loss` and `warnings`. It rewrites `environment.search_path`
of `service.toml` (atomically, under the transaction lock, unknown keys kept)
and replaces the daemon job in one journaled transaction (record operation
`refresh_path`, record schema 6, which a release that writes schema 5 refuses):
the adoption verdict is taken before any effect and again before the
replacement, readiness requires a new daemon process, and a failure restores
the previous path and job. Worker jobs are never touched and the accepted-risk
ledger is never written. `status`, `check` and the reports of the other service
commands show a pending refresh as `pending_transaction` with `operation`
`refresh_path`, and `rolling_back: true` (omitted otherwise) once its rollback
has begun. A pending upgrade also carries `previous_version`, the version it
started from (omitted for the other operations); the text of `status` shows it as
`pending upgrade <to> from <previous>`, and rolling the upgrade back means
`pohunek service upgrade --from <prefix>/libexec/pohunek/<previous version>`. It fails with `service_upgrade_sessions_at_risk` or
`service_upgrade_store_unusable` as an upgrade does, with
`service_daemon_job_missing` when a run before `registering` rolls back for lack
of a registered daemon job, or the first run before any record exists finds none
(the latter changes nothing), with `service_refresh_config_changed` when `service.toml` was edited
during a pending refresh that has to act on it (a refresh at `ready` only
clears its record after reading and verifying the file, without comparing it with
the refresh, and `uninstall` removes the installation without needing the file to
match the refresh), and `service_refresh_pending` is what `install` and
`upgrade` return while a refresh that reached the job replacement is pending
(`pohunek service refresh-path` without `--dry-run` resolves it), and what a
dry run returns over any pending refresh, because a dry run never resolves one.
While a refresh is stopped at `registering` or rolling back from `registering` on, and its active version directory is refused or missing, `refresh-path` fails with `service_version_untrusted` (wrapped in `service_rollback_failed` when the first run was a rollback), and `upgrade --repair --from <release of that version>` publishes the directory before it returns `service_refresh_pending`, whose message states that the directory was restored; the record and `service.toml` are untouched and `refresh-path` then finishes the refresh or its rollback.
A daemon job that vanished after the record exists makes the run roll back:
before `registering` the file is restored and the record cleared and the run
returns `service_daemon_job_missing`; from `registering` on the rollback
registers the job again from the previous definition, after the same adoption
gate, and the run still returns `service_daemon_job_missing` with the record
cleared. A rolling-back record that a rerun finds without a job is finished the
same way. Each such message states the effects the run had, read from the files
after it ended: `service.toml` unchanged, put back on the previous search path,
or recording the new search path; the record absent, cleared, `rolling_back` or
kept; and whether the rollback registered the job again. It states none when a
file cannot be read. The hint names `pohunek service upgrade --from
<prefix>/libexec/pohunek/<active version>`, which registers a missing job again
from `service.toml` and keeps the settings and the live sessions, with
`pohunek service uninstall` as the alternative; after a rollback that registered
the job again it says to rerun `refresh-path`. The store and preflight refusals
carry the codes of the upgrade errors (`service_upgrade_store_unusable`,
`service_upgrade_preflight_failed`) with texts that name the installed release. It fails with `service_search_path_refresh_failed` when the
login shell fails, hangs or prints junk or yields no usable directory (the
fallback list never replaces a recorded path), `service_search_path_unmanaged`
where the host records no path, `service_refresh_blocked_by_transaction` while an
install or upgrade record is pending, `service_refresh_config_schema` for a `service.toml` of another
schema than this release writes (nothing is rewritten; run the installed release's own
`refresh-path` when it has the command, otherwise stop the live sessions, save a copy of `service.toml`, uninstall, install the
installed version from its archive with the same `--prefix`, reapply the non-default settings
except the recorded `search_path`, and rerun the upgrade), and `service_not_installed`. A set but relative `$SHELL`, or a
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

`pohunek service install --repair` and `pohunek service upgrade --repair` are the
supported recovery for a refused (`service_version_untrusted`) or missing version
directory. The flag acts only on the directory of the version the run installs: a
refused entry is renamed to `.pohunek_refused-<version>-<random>` in the versions
directory (a rename that never follows a link and moves any entry type), the
staged and probed release copy is published with the no-replace move, and
`service.toml` is never rewritten. The renamed-aside entry is kept: it is not a
version, is never swept or collected and is never a `--from` source; the upgrade
and uninstall reports list the entries as `refused_entries` (omitted when none
exists). A missing directory is published without the flag; a refused one
without it fails with the same code and changes nothing. A directory that passes
the installed-directory check is never replaced, and one that differs from the
release still fails with `service_version_conflict`. An upgrade whose `--from`
probes as the active version then makes the daemon job run from the restored
directory (a missing job is registered again, a stopped one started, a running
one restarted as a new process when the directory was restored or `--repair` was
given, after the adoption preflight judged the release; `service_upgrade_sessions_at_risk`
and `--accept-runtime-loss` apply and a refusal changes nothing), and never
stops or retires worker jobs. The success reports of `install` and `upgrade` carry an additive `repair`
object (`version`, `restored`, `set_aside`, `daemon` as `none`, `installed`,
`started`, `restarted` or `kept`, a resumed transaction that had already replaced the
daemon, which still runs from the moved entry until `upgrade --repair --from` is
rerun; `null` without the flag and when nothing was repaired).
`UpgradeReport.unchanged` states that the active version did not change; a
repair of it can still restart the daemon. The trust claim is that of `service install --from`
(the operator chose the release, each binary answers `--version`, a sealed
release's `runtime/` files are present) and no digest of the binaries is
verified; running processes are not attested. When an `uninstall` finds live
worker jobs and an untrusted or missing active version directory it fails with
`service_version_untrusted`, a message naming the active version and a hint with
the `service upgrade --repair` command; no new error code exists. The
[debug daemon runbook](knowledge/runbooks/debug-daemon.md#active-version-directory-refused)
is the procedure.

`pohunekd ensure-official-runtimes` is the offline runtime step of the release;
`--dry-run` computes its plan without changing anything. `--dry-run --detect` also
plans every runtime of the release catalog for this platform whose program is on
the daemon's search path, each with the reason `detected` (a runtime the host uses
keeps its other reasons). `--detect` without `--dry-run` is a usage error. A host
without an installed `service.toml` is planned with the launch environment `service
install` gives its daemon: the default allowlist over the process environment. An
existing service file that is unreadable or malformed still refuses, and a plain
dry run still refuses a missing file. Its
`gaps` list, one per runtime with `runtime`, `code` and a `detail` that names the
one next step: `agent_not_on_search_path` (the program is not on the search path;
the runtime is planned only when the host uses it), `config_home_missing` (the
declared config home does not exist; the package is still planned and the step
never creates the home), `config_home_is_symlink` (the config home is a symlink; hooks
are installed only into a real directory, so no integration is planned into it and
the step follows no symlink), `integration_path_untrusted` (the installer refuses the
path: it is not a normalized absolute path to a real directory), a profile's own
resolution error such as `runtime_not_installed` (a profile pinned to a package the host
cannot serve, which `integration.install` would reject), `runtime_served_by_other_package` (another package serves
the runtime id; it is left untouched), `official_package_unavailable_for_platform`
(the release catalog lists no package for this platform; a refusal without
`--detect`, except for an installed official package of a runtime that is not
reserved, such as Pi, where it is a gap in every mode, and a gap `--accept-gap` names), and
`official_package_not_serving` (an official package of such a runtime is
installed but not selected, so the step leaves it, or the selected package is disabled
(whatever its version order against the release's, the release's own package
included), or the selected version cannot be ordered
against the release's and does not load, so the step leaves that too; the detail names the
command by cause: `pohunek plugin update` for a package that is not selected,
`pohunek plugin enable <package> --digest <digest>` for a disabled package (any runtime,
Pi included; `plugin update` would refuse a newer selection as a downgrade), and
`pohunek plugin install` or `plugin select` for an unordered selection that does not
load; when an enabled version still serves the runtime beside a disabled release
package it keeps serving and the gap names the release package; the
[case table](#disabled-release-package-case-table) gives the outcome for every state of
the selection and of the release package's root, the refusal of an altered root and the
kept newer Pi version included, and the unordered selection reports
`plugin_update_version_unordered`; under `--detect`
the gap is dropped with a runtime the detecting plan drops, that is one nothing uses,
no profile has as its base, and whose program is off the search path), and
`plugin_update_version_unordered` (the selected version of an official package, of
a reserved runtime or not, cannot be ordered against the release's and is enabled and loadable, so it is kept,
as `plugin update` refuses to replace it), and
`plugin_update_same_version_other_digest` (the registry already records the release
package's id and version under another digest, whatever its flags, source or selection,
so the release's archive cannot be installed beside it; the enabled, loadable selection
that serves the runtime is kept, status `kept` with `release_version` the shared
version, nothing is written; a newer Pi selection is kept silently. The detail names
`pohunek plugin uninstall <package> --digest <digest> --yes` of the recorded package only
when that package is the verified, selected one of a reserved runtime that a profile,
session or installed hook makes the plain upgrade plan, and no session or host profile pins
it and no unpinned profile is bound to it by the upgrade (no command releases a pin); in
every other state it names no command: the selected package keeps serving, and the fix is a
release that gives the package a new version. For Pi the detail adds that the upgrade does
not install a Pi package again once none is installed.) The first two of these four name the command
that updates or repairs the package by hand. A gap never blocks an apply, and `gaps` is empty
without `--detect` except for those four and the gaps `--accept-gap` names. The step also keeps an installed official package of a runtime that is
not reserved (Pi) current: when one is installed from the official catalog,
selected and enabled, and the release lists a package of the same id, the plan
installs and selects the release's version (reason `installed`) and the profiles
pinned to the older official version are repinned; unpinned profiles stay unpinned,
an absent package is never installed, nothing the operator turned off is changed,
and a selected version newer than the release's is kept (status `kept`, with
`release_version` naming the release's older one) and never downgraded. For every
runtime, reserved or not, a selected version that cannot be ordered against the
release's and is enabled and loadable is kept the same way (status `kept`, with
`release_version` naming the release's version). A host profile pinned to another
digest than a kept selection stays as it is and is reported `kept` with the precise
reason (`pinned_version_unknown`, `pinned_to_non_official_package` or
`pinned_to_other_package`) when the pin has one, and `pinned_differs_from_kept_selection`
for an official version of the same package. `integrations` lists the config homes the installer reinstalls hooks
into: without `--detect`, the homes that already hold hooks; with `--detect`, each
existing home of a planned runtime whose hooks the setup installs, whether or not
hooks are present yet (Pi declares none). Each distinct home appears once. `reason`
values are `profile`, `session`, `integration`, `detected` and `installed`. Two codes name a
runtime the catalog lists more than once for this platform: `official_package_listed_more_than_once`
(a profile, a session or an integration uses the runtime, so the
plan without `--detect` refuses it with this code and `--detect` lists it as a gap) and
`catalog_lists_runtime_more_than_once` (only the detection found the runtime, so
no apply plans anything for it and nothing is refused). `--accept-gap <runtime>:<code>`,
repeatable after an optional `--dry-run` and a usage error with `--detect`, where `<code>` is the
refusal's code (the `code` of an entry of `refusals`, not of a gap), reports exactly that
refusal as the gap the detection reports for that condition instead, and every other refusal still fails the step; `pohunek setup`
passes the `official_package_unavailable_for_platform` gaps its preview showed this way when it
installs the service, so the service installs and the runtime stays a gap, while a used
runtime the catalog lists more than once still stops setup before any change. The report layout
stays at `report_version` 1.

<a id="disabled-release-package-case-table"></a>
**Disabled release package: case table**

The step never installs, selects or enables a package you disabled, and never
re-enables the release's own package when it is recorded and disabled
(`plugin install --no-enable`, `plugin disable`). The plan and the apply give the
same outcome for a row. "S" is the selected version of the release package's id; "R"
is the release's own package, recorded, official and disabled, with the state of its
root in the third column. A reserved runtime is `claude`, `codex` or `hermes`, used by
a profile; Pi is planned only while an enabled S exists, or under `--detect`.

| Outcome | Meaning |
| --- | --- |
| refused R, refused S | Refusal `package_root_invalid`; its detail names `pohunek plugin uninstall <package> --digest <R or S digest> --remove-modified --yes`. |
| kept | Package entry `kept` with S; nothing is installed, selected or enabled, and no gap is reported. The apply pins an unpinned profile of a reserved runtime to S, as for any kept selection. |
| kept, gap enable R | `kept`, and the gap `official_package_not_serving` naming `pohunek plugin enable <package> --digest <R digest>`. |
| kept, gap restore, then enable R | As kept, gap enable R, with `plugin install` of R's archive before the enable. |
| kept, gap unordered | `kept`, and the gap `plugin_update_version_unordered`. |
| gap enable R, gap enable S | No package entry; the gap `official_package_not_serving` naming `plugin enable` for that digest. |
| gap restore, then enable R | As gap enable R, with `plugin install` of R's archive before the enable. |
| gap repair S | The gap `official_package_not_serving` for an enabled, unordered S that cannot load: `plugin install` of that version's archive, or `plugin select` of another installed version. |
| refused, no server | Refusal `release_runtime_catalog_unavailable`: S is kept and no installed package serves the runtime. |
| gap doctor | The gap `official_package_not_serving`, pointing to `pohunek plugin doctor`. |
| gap not selected | The gap `official_package_not_serving` saying the package is installed but not selected, with `pohunek plugin update`. |

| # | Selected version S | Root of R | claude, codex, hermes | Pi | Pi with `--detect` |
| --- | --- | --- | --- | --- | --- |
| 1 | enabled, loads, older than R | altered | refused R | refused R | as Pi |
| 2 | enabled, loads, newer than R | altered | refused R | kept | as Pi |
| 3 | enabled, loads, older than R | intact | kept, gap enable R | kept, gap enable R | as Pi |
| 4 | enabled, loads, newer than R | intact | kept, gap enable R | kept | as Pi |
| 5 | enabled, loads, older than R | missing | kept, gap restore, then enable R | kept, gap restore, then enable R | as Pi |
| 6 | enabled, loads, newer than R | missing | kept, gap restore, then enable R | kept | as Pi |
| 7 | enabled, loads, version unordered against R's | not examined | kept, gap unordered | kept, gap unordered | as Pi |
| 8 | enabled, root missing, unordered | not examined | gap repair S | gap repair S | as Pi |
| 9 | enabled, root altered, unordered | not examined | refused S | refused S | as Pi |
| 10 | enabled, root missing, older than R | altered | refused R | refused R | as Pi |
| 11 | enabled, root altered, older than R | altered | refused R | refused R | as Pi |
| 12 | enabled, root missing, newer than R | altered | refused R | refused, no server | as Pi |
| 13 | enabled, root altered, newer than R | altered | refused R | refused, no server | as Pi |
| 14 | enabled, root missing, older than R | intact | gap enable R | gap enable R | as Pi |
| 15 | enabled, root missing, older than R | missing | gap restore, then enable R | gap restore, then enable R | as Pi |
| 16 | enabled, root missing, newer than R | intact | gap enable R | refused, no server | as Pi |
| 17 | enabled, root missing, newer than R | missing | gap restore, then enable R | refused, no server | as Pi |
| 18 | disabled, older than R | not examined | gap enable S | gap enable S | as Pi |
| 19 | disabled, newer than R | not examined | gap enable S | gap enable S | as Pi |
| 20 | disabled, older than R, root altered | not examined | refused S | gap doctor | refused S |
| 21 | none selected | altered | refused R | gap not selected | refused R |
| 22 | none selected | missing | gap restore, then enable R | gap not selected | gap restore, then enable R |
| 23 | none selected | intact | gap enable R | gap not selected | gap enable R |

How to read it:

- A disabled S (rows 18-20) is judged alone, whatever its version and even when it is
  R itself; R is not examined. An enabled S whose version cannot be ordered against
  R's (rows 7-9) is judged alone too. For these rows the table shows the outcome with
  R's root altered, the state that would be refused if R were examined.
- Otherwise R is examined. While S loads it keeps serving beside R, and the plan
  reports R's state: an altered root is refused, an intact or missing root is the gap
  that names `plugin enable` (rows 1-6). Pi keeps a newer S that loads silently and
  does not examine R beside it (rows 2, 4 and 6). When S cannot load, or there is none,
  R decides alone (rows 10-17 for a reserved runtime, rows 10, 11, 14 and 15 for Pi,
  and rows 21-23 for a reserved runtime and for Pi under `--detect`); Pi keeps a newer S that cannot load and then refuses it because no
  package serves the runtime id (rows 12, 13, 16 and 17).
- A disabled package the plan examines (R, or a disabled S) is refused when its root
  is altered, gets the restore-then-enable hint when its root is missing, and the
  enable hint when it can be enabled. When a third-party Pi package serves the
  runtime id (a third party cannot claim a reserved id) the plan cannot enable it: a
  disabled R beside an older enabled Pi S that cannot load is refused with
  `package_runtime_conflict` and the detail "the disabled package cannot be enabled
  while another package serves this runtime id", which `--detect` reports as the gap
  `runtime_served_by_other_package`; a disabled Pi S is examined only under `--detect`
  (see the next item), where the same claim is that gap.
- Pi without an enabled S (rows 18-23) is not planned without `--detect`: its package
  gets only the gap of the managed-runtime check (`plugin enable`, `plugin doctor`
  when the package cannot be enabled, or `plugin update` when it is not selected).
  `--detect` lists Pi, so it is judged as a reserved runtime is, which changes rows 20-23.
- `--detect` changes no cell of a reserved runtime. A runtime nothing uses whose
  program is off the search path is dropped by the detecting plan with its refusals
  and gaps, leaving only `agent_not_on_search_path` (every reserved row; the Pi rows
  without an enabled S, 18-23).
- S cannot have the release's version under another digest while R is recorded: the
  registry holds one digest per package version, so a second digest is refused with
  `package_identity_installed`. When the registry records the release's id and version
  under another digest and an enabled, loadable S serves the runtime (R is not
  recorded), the plan and the apply keep S (status `kept`) with the gap
  `plugin_update_same_version_other_digest` and write nothing, for a reserved runtime and
  for Pi, except that a newer Pi S is kept silently, with no gap, as beside any release
  package it would replace; a disabled S is row 18-20's outcome. When no enabled official S
  serves beside that recorded package (the holder H), the release's archive still cannot be
  installed, so the plan and the apply agree and write nothing (the holder table below).

The holder table lists what the plan, the apply and `--detect` say for a runtime whose
release package's id and version the registry records as H under another digest and
that no enabled, loadable official S serves. A runtime nothing uses whose program is off the
search path is dropped before it is judged, as in the table above. Plan and apply are one
computation (a left or refused runtime has no planned entry), so the table has two columns:
plan and apply (P/A), and `--detect` (D); every cell below holds in plan, apply and
`--detect` unless a row says otherwise, and the registry and the host profiles are
byte-identical afterwards.

One function renders every hint of the table, for the planned-runtime check and for the
managed-runtime check of Pi alike. The obstacle O is the selected digest, or H when nothing is
selected. The hint names, in this order: the release of every pin on O (below), the removal of
O (`pohunek plugin uninstall <package> --digest <O> --yes`, with `--remove-modified` exactly
when O's root is altered), and the follow-up after the removal. The follow-up is "run the
upgrade again" when the plain upgrade plans the runtime again (a reserved runtime that a
profile, hook or a session that does not pin O uses, or a Pi that keeps another enabled
official selection); for a Pi that keeps none it is the install of the release's package,
`pohunek plugin install runtime/packages/pi.tar.zst --catalog runtime/runtime-catalog.json --yes`
(`plugin update` with the same arguments when another Pi version is recorded, because
`install` selects only the first), whatever uses Pi, because the command installs and
selects by itself.

| State beside H | Outcome | Hint |
|---|---|---|
| Nothing selected, or H selected and its root missing or not loading; H enabled or disabled | gap `official_package_not_serving`, no package entry, profile left unbound | remove H, then the follow-up; Pi: install or update step |
| H unselected or selected, root altered | refusal with the root-invalid code | `--remove-modified` removal of H, then the follow-up |
| H selected and not official (hand-edited registry) | gap; refusal when its root is altered | remove H (`--remove-modified` when altered), then the follow-up (H is the obstacle whatever its source) |
| Another version X selected, enabled, root missing or not loading | gap | restore X with `plugin install`, or `plugin select <package> --digest <H>`; or, while H is not pinned, remove H with the follow-up |
| X selected, root altered | refusal for X | removal of X with `--remove-modified`, then the upgrade again, which names the removal of H |
| A non-official S selected | gap | `plugin select <package> --digest <digest>`; or, while H is not pinned, remove H with the follow-up |
| Pi without an enabled official selection (not planned without `--detect`), nothing selected or a non-official S | gap (plan, apply and `--detect`); an altered H is a gap in plan and apply and a refusal under `--detect` | the same renderer, so the Pi row above applies and no command fails beside H |
| Newer Pi X selected, root missing or altered | the existing refusal for X | unchanged |

A pinned O (a host profile holds its `digest`, usually beside its `package`, or a session record holds it) cannot
be removed: the registry refuses the removal while the pin stands. The hint then names the
pin first: "the host profile `<name>.toml` pins it" or "a session record pins it", "so the registry refuses
its removal until the pin is released", with the exit "remove the `package` and `digest`
lines from `<name>.toml` in the host profile directory" (the `digest` line is what pins) or "remove the sessions launched from
it" (no command releases a pin), and then the removal and the follow-up. A reserved runtime
that only the pinning session used gets no "run the upgrade again", because the plain upgrade
no longer plans it once the sessions are gone. A hint that offers
the removal of H beside another obstacle X does so only while nothing pins H. A reserved runtime that
is used only by `--detect` gets no command in a gap and the removal in a refusal; a reserved
runtime nothing uses gets "the fix is a release that gives the package a new version". A Pi
always gets the commands, because its install and select run by hand whatever uses it.
A profile that pins another installed version P keeps the command: the uninstall is accepted
with P retained, and the next apply installs the release's archive and repins the profile
from P.

The scenarios are the twelve
`<runtime>_judges_the_disabled_release_package_by_the_selection_state_part_<0-2>` tests
(claude, codex, hermes and pi; rows 1-23 are split over the three parts; plan,
`--detect` plan and apply, registry unchanged, profile pinned only in the kept rows),
`a_disabled_release_package_that_another_package_serves_is_refused_beside_an_unloadable_selection`
(the claim), `a_disabled_pi_selection_beside_a_third_party_server_is_a_gap_without_detect`
and `a_disabled_pi_selection_beside_a_third_party_server_is_reported_as_claimed` (a claimed
S), all in `crates/daemon/tests/ensure_runtimes.rs`, and
`install_refuses_an_identity_installed_from_another_archive` in
`crates/package/tests/registry.rs`. The holder cells are rows of one table in the same file,
run by the eight `the_holder_contract_table_holds_for_<runtime>_<first|second>_half`
tests (claude, codex, hermes and pi, one half of the rows each): each row runs plan, apply and `--detect`, counts
that the hint names exactly the commands the row lists, in order, runs every one of them as
the CLI sends it (the registry refuses the removal while the pin stands and accepts it after
the pin is released), and ends with the release installed and the profile bound (reserved),
the release's package present (Pi) or the chain of the next hint. Beside the table:
`an_unselected_altered_pi_holder_is_a_gap_without_detect_and_a_refusal_with_it`,
`a_runtime_planned_only_by_detection_with_an_altered_holder_is_refused_with_its_removal`,
`a_runtime_planned_only_by_detection_beside_a_holder_gets_no_uninstall_command`,
`a_newer_pi_selection_without_its_root_beside_a_holder_keeps_its_refusal`,
`a_detecting_plan_drops_an_unused_runtime_before_judging_its_holder`, the newer-Pi keep
`a_newer_pi_selection_beside_a_holder_of_the_release_version_is_kept_silently`, and the pinned
older version `a_profile_pinned_to_an_older_version_beside_the_holder_gets_the_whole_remedy`.

`pohunek service upgrade` and `pohunek service check` of an existing service run
a read-only preflight before any effect by invoking `pohunekd upgrade-preflight`
of the staged archive. That mode takes no other argument, is an internal contract
between the two binaries of one archive rather than a public client interface,
and prints one JSON document (`PreflightReport` in `crates/service-config`,
`report_version` 1): `store` (`state` `missing`, `up_to_date`, `would_migrate` or
`refused`, with `schema_from`, `schema_to`, `records`, `error_code`, `error`) and
`sessions` (per live session `session_id`, `verdict` `adoptable`,
`would_lose_recovery` or `would_not_be_adopted`, a stable `code`, and a human
`detail`, plus the owner-set `name` when there is one, and, for an at-risk
verdict of a loaded record whose code the daemon proves stable across its
startup, `acceptance_scope`: `v1:` and a SHA-256 digest of the facts a durable
acceptance stays bound to; it is absent for every other verdict and a reader
treats an absent or unknown scope as not accepted), `stopped_sessions` (the
same verdicts, plus `already_not_resumable`, for sessions whose runtime has ended
but that keep native resume or fork; at risk under the same flag), `runtimes` (absent when the release carries no
`runtime/` tree; otherwise the `EnsureReport` of the release's official runtime
plan with `packages`, `profiles`, `integrations` and `refusals`), and
`unmanaged_workers` (ids of live worker journals the store has no record of; the
running daemon does not manage them either, so they never refuse the upgrade),
and `profile_warnings` (host profiles that load with a reduced guarantee, ordered
by `profile`, each with a `code` and a `detail` naming the profile and its
manifest; the code `profile_without_screen_readiness` marks an override manifest
without the idle rule on a screen region that its `screen_idle` base needs). The
field is omitted when empty and never refuses the upgrade or changes a verdict. A session that is neither `adoptable` nor `already_not_resumable` fails the command with the CLI error
code `service_upgrade_sessions_at_risk` unless `--accept-runtime-loss` is given;
a `refused` store fails it with `service_upgrade_store_unusable` and a preflight
that cannot run or be parsed with `service_upgrade_preflight_failed`, neither of
which the flag overrides. The official runtime step adds five codes, none overridden by `--accept-runtime-loss`: `service_upgrade_runtimes_refused` (the release's plan cannot be applied; nothing was stopped), `service_upgrade_runtimes_failed` (the release's runtime step did not produce a finished report), `service_runtime_backup_invalid` (a registry, catalog state or host profile cannot be backed up exactly), `service_staged_runtime_invalid` (the release's `runtime/` tree is not the layout the release assembles, or a sealed release lists a `runtime/` file it does not carry), and `service_upgrade_runtimes_unconfirmed` (the step's daemon run could not be ended and its exit confirmed, so the upgrade is kept and not rolled back). The `--json` success reports of both commands carry
`preflight` (the report, or `null` when no preflight ran) and
`accepted_runtime_loss` (boolean, true only when sessions were at risk and the
flag accepted them), and the omitted-when-empty groups `previously_accepted`
(at-risk verdicts an earlier upgrade accepted with the identical scope, which do
not block) with `previously_accepted_at` (that upgrade's version),
`durable_acceptance` (accepted now and recorded for the next upgrade) and
`one_time_acceptance` (accepted now, with no scope to bind them). The completed
upgrade replaces `<state>/pohunek/service-accepted-risks.json` with the scoped
verdicts of its final gate; `service check`, a refusal, a failure and a rollback
never write it, and an unreadable or unsafe file fails with
`service_accepted_risks_invalid`. The preflight reads the store, the worker
journals and the process table only; it never connects to a worker socket. A
live session whose worker journal names a release before 0.33.1, or no release,
is `would_not_be_adopted` with code `worker_marker_legacy`. A live session that
can resume or fork natively and whose launch pin names a runtime source the new
binary does not serve (resolved as resume resolves it, against that binary's
built-in runtimes and the installed packages: a built-in it does not ship, or a
package digest that is not installed) is `would_lose_recovery` with code
`runtime_source_not_served`; its `detail` names the resume error and the
migration to a new session. A built-in pin that the official package serving its
alias adopts is `adoptable` instead. Every
live session, whether or not it can resume, is also judged with the mutation
guard: one whose runtime resolves nowhere on the new binary (a built-in it does
not compile in while no package serves the alias, `runtime_builtin_removed`, or
a runtime that is not installed) is `would_lose_recovery` with the same code,
and its `detail` says every interaction (input, attach, resize, rename,
metadata) is refused until the runtime is installed, naming the official
package install for a removed built-in. A live or stopped session whose host
profile resume would refuse (the profile is missing, or its frozen revision
changed by more than the migration pin) is `would_lose_recovery` with resume's
code (`agent_profile_missing`, `agent_profile_changed` or
`agent_profile_revision_unavailable`), and a stopped session record the new
binary cannot load is `would_lose_recovery` with `record_unreadable`. A recovery
binding that waits for the runtime registry to complete its launch spec is
repaired first, as startup repairs it, over the profiles and runtimes the new
daemon will have (read-only: nothing is written), and is then judged like any
other binding. A binding the repair cannot complete (the agent no longer
resolves, runs on another runtime, declares no native resume or another
reference kind, or its launch pin names a source the new daemon does not serve)
is `would_lose_recovery` with `native_recovery_unrepairable`; it has no
acceptance scope, so accepting it is one-time. `native_recovery_unverified`
reports the same binding only from a daemon older than this repair, which judges
a rollback. A session
whose frozen program resume would not find is `would_lose_recovery` with
`agent_binary_missing`. A stopped session whose conversation is absent from its declared store
(`agent_native_reference_missing`), or whose stored reference is malformed, is
`already_not_resumable` with resume's code when the upgrade does not change the
check, which then needs no flag. A relocated store (`agent_native_store_relocated`)
or a store-condition refusal (`agent_native_reference_missing` from an unresolvable
root, a scan limit or a changed store) is `would_lose_recovery` with resume's code,
because the running release may detect it differently. A reported reference (one the runtime reports rather than the launch
assigns) keeps its check across the upgrade only when the preflight can show it identical: a package pinned to the session's digest, which
content addressing fixes, or a built-in that the upgrade adopts from a 0.34.0 to 0.34.2 store (store schema 5) whose adopted package declares
the existence check of the v0.34.2 golden. Any other reported reference is `would_lose_recovery` with resume's code and a detail that names the
reason: a built-in the upgrade does not adopt, an adopted package from a store that is not schema 5, or a package whose check differs from the
golden. Hermes has no golden here, so its reported references are fail-closed too. A stopped session whose reference the preflight budget
reached before its check is `would_lose_recovery` with `reference_unverified_within_budget`: the reference is not known to be present, so it is
never adoptable, and the detail gives the number of such sessions in the report. The budget is 20 seconds by default, bounds the version probes and
the reference checks together, and is raised with `service upgrade|check --preflight-budget <SECONDS>` (1 to 300; the CLI's preflight deadline
grows with it). The flag reaches only this release's upgrade preflight: a rollback's preflight runs the previous release's daemon with the default
budget. The flag is sent to the daemon as `--probe-budget-secs` only when it differs from the default. An installer user (`packaging/install-daemon.sh`) cannot pass a budget, so it runs the archive's own commands: `<archive dir>/pohunek service check --preflight-budget <SECONDS>`, then `<archive dir>/pohunek service upgrade --from <archive dir> --preflight-budget <SECONDS>`. The wrapper runs both under `pohunek service lock` and passes `--accept-runtime-loss` to both when given; after a successful run it prints the hook asset step (`pohunek integration doctor` and `pohunek integration install --agent <agent>`), which a direct `service upgrade` does not print.  For a runtime that declares a version probe, the
preflight runs the same isolated, bounded probe resume runs (the frozen program
with the probe's arguments, a private `HOME` and working directory), so
`service check` executes those programs. It writes no pohunek state. Identical
probes (same program, `PATH` and package) run once, and all probes of one run
share a budget below the installer's preflight deadline; a session whose probe
the spent budget cuts off is `would_lose_recovery` with `probe_budget_exhausted`.
The release plan is judged only when the installer passes `--release-plan` (the
upgrade's own gates and `service check` of an upgrade); without it, as for a
rollback, which never applies the plan, the preflight judges the installed
packages only. The preflight runs with the `PATH` the daemon job runs with, byte
for byte: the one `service.toml` records; else, on Linux, the systemd user
manager's `Environment` `PATH`. On Linux a drop-in of the daemon unit replaces either one (an `Environment=PATH=` entry, or for the manager's `PATH` an
`ExecSearchPath=`; the install replaces the unit file itself and keeps its drop-ins); a drop-in that replaces a recorded `PATH` is named in a
`warning:` line. A drop-in's `Environment=` also replaces the manager's value of an allowlisted store variable the daemon reads (such as `CLAUDE_CONFIG_DIR`),
and `UnsetEnvironment=` removes it. The command is refused with `service_manager_environment_unavailable`, naming the file or directive, and
before any effect, when: a drop-in sets, resets or removes a bootstrap key (`HOME`, `XDG_*`) or resets `Environment=`; an `EnvironmentFile=` sets `PATH`, a bootstrap key or an
allowlisted variable (move it to a drop-in `Environment=` line), is a wildcard, or is an unreadable or non-regular required file; `PassEnvironment=` names `PATH` of an unrecorded
job or an allowlisted variable no drop-in assigns; `UnsetEnvironment=` names `PATH` or a bootstrap key; the unit is masked or broken; a setting is in a form the CLI does not
read (an escape sequence, a specifier, a quote never closed); the bus is unreachable or the unit's properties cannot be read, whether or not the job records its `PATH`
(`--accept-runtime-loss` does not override it); or a drop-in of a loaded unit could not be resolved the same way once the unit is removed, which the loaded gate refuses upfront because a
rollback or config migration removes the unit and resolves again from its drop-ins alone (a unit with drop-ins but no unit file yet is read from them; see
[the runbook](knowledge/runbooks/update-after-release.md#the-journal-of-an-interrupted-upgrade) for the full list of refusals and the reading rules, including the odd/even rule for a trailing
backslash run). On macOS the job's `PATH` is launchd's active one:
the domain's `environment` `PATH` (`launchctl print gui/<uid>`, set by `launchctl
setenv`), else the `default environment` of the loaded job (`launchctl print
gui/<uid>/<label>`), else, for a job that is not loaded, the launchd default an
upgrade journaled while the job was loaded and the host has not rebooted since (the
journal records the boot session UUID; after a reboot the persisted configuration decides only
when `user.plist` was last modified before the current boot; a forward gate refuses one modified later, or without a
readable time, and a rollback goes on with it as a stand-in and a `warning:` line, or with launchd's standard `PATH`
when its content cannot be read; without a marker the persisted value must also equal the journaled one),
else, with no journaled default, the persisted launchd configuration when it is older than the boot (by the
configured margin; no `user.plist` means launchd's standard `PATH`), and otherwise (newer than the boot, or a time that cannot be read) a
forward gate is refused and a rollback goes on with the persisted value and a `warning:` line. A rollback judges with the same order,
so the `PATH` it uses is the one the restored job gets where launchd can still
report it; the `warning:` line says that the `PATH` is a stand-in, because no launchd default `PATH`
that is known to apply was available. An
unrecognized `launchctl print` output refuses the upgrade with `service_manager_environment_unavailable`.
On macOS a job whose launchd variables cannot be read takes none: any job when launchd has
no `gui/<uid>` domain, as without a GUI session (a job without a recorded `PATH` then takes the
persisted launchd configuration's `PATH`). `service check` is not refused: it prints a `warning` that
native references are judged at the default store locations, so a verdict may read
`already_not_resumable` or `would_lose_recovery` for a conversation stored under a variable-set
location, or `adoptable` for one present only at the default location. When such a judgement
covers a stopped session, whose native reference check reads a store location, `service upgrade`
and a rollback refuse it with `service_manager_variables_unread` (macOS only) before any effect, unless
`--accept-runtime-loss` is given; a judgement without stopped sessions does not depend on those
variables and proceeds with the warning. The
preflight resolves programs with the daemon's own resolver, which skips an
untrusted candidate the same way at launch. See
[update after release](knowledge/runbooks/update-after-release.md).

Identity hooks prefer the owner-private worker endpoint so an accepted launch
or active identity is retained while the daemon is unavailable. Notification
hooks continue to use the public daemon socket; notifications produced during a
daemon outage are not durable. `POHUNEK_DAEMON_ID` remains additive compatibility
data but is not the stable runtime identity and must not be used for
self-feedback decisions by new clients.

When the worker-private native-identity claim cannot be delivered, shipped
Codex and Claude hooks fall back to the public `session.report_native_id`
method, which the daemon attests against the session's PTY root (see the
native-id report attestation rules above). A hook is a child of the agent, so
that fallback is refused with `native_report_unattested`; the native reference
is recorded only through the worker hook socket. The origin-session guard deliberately allows
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
`seq` is a decimal string. A session keeps the protocol version of its launch
(`POHUNEK_PROTOCOL_VERSION`), so a session launched under protocol 3 keeps
reporting after a daemon upgrade: its hooks send the range `3..=3` and spell the
worker instance key `runtime_id` in `session.report_native_id`, which the
daemon serves through the protocol 3 adapter. `pid` is the OS process id for the active nested agent. When present, the daemon
binds the active claim to that process and clears the claim when procwatch sees
the process exit. The shipped integration state hooks use
`POHUNEK_INTEGRATION_VERSION=15`, run their interpreter in isolated mode (`-I`,
so the session working directory never shadows the standard library), read the
worker instance from `POHUNEK_WORKER_INSTANCE_ID`, read
provider JSON through a bounded direct pipe without staging it on disk, and send the hook process's parent PID on
`SessionStart`. An installed hook whose content differs from the shipped asset,
for example one of an earlier release that reads another environment name,
is reported by `integration.status` as
`outdated` with `reinstall` recovery, and by `integration.doctor` as an asset
finding, until it is reinstalled. `POHUNEK_WORKER_INSTANCE_ID` is the only
worker-instance marker: a worker sets it on every child and strips every
inherited `POHUNEK_*` variable from them, and the hooks, the process inspectors
and the process sweep read no other name. Workers of releases before 0.33.1
mark their children with another name, so the daemon does not adopt such a
live worker (`incompatible`, `worker_marker_legacy`), and a sweep of a
session's runtime never signals a process that carries that session's
`POHUNEK_SESSION_ID` and `POHUNEK_WORKER_ID` (set only by a worker) but no
worker-instance marker: it leaves the process running and the cleanup
unconfirmed. A lifecycle hook the daemon runs for the session carries no
`POHUNEK_WORKER_ID` and is ignored by the sweep. An operation refused on such a
session (`session.stop`, `session.resize`, input, attach) keeps the state's code
`worker_protocol_incompatible`, names `worker_marker_legacy` in `msg`, and its
`recover` says to remove the session with `pohunek session rm` and accept
losing its runtime: the release that started such a worker is outside the
supported upgrade window, and the daemon does not notice when it ends.

`session.report_agent` also accepts two optional fields that bind a report to
the agent process. `pid_start_identity` is the kernel process-start identity of
`pid`, spelled as a decimal string like in `session.report_native_id`; the
daemon reads the identity of the process behind `pid` and answers
`recorded: false`, changing nothing, when the report names an identity without a
`pid`, when no process runs under the `pid`, or when the process started
differently. A confirmed identity binds the active claim to that exact process
without waiting for procwatch to observe it. `monotonic_sequence` is a decimal
string on the session host's `CLOCK_MONOTONIC` clock, the clock the worker hook
path orders reports with. It is a separate field from `seq`, which stays the
wall-clock value that orders the memory-only path, and the daemon does not
compare the two. The shipped Claude, Codex and Hermes reporters send both
fields on their daemon-socket fallback. A report that names a `pid` without a
`pid_start_identity` while the session's active claim is journaled by the worker
is bound by the identity the host reports for the `pid` now, so it replaces that
claim only from the same process with a newer `seq`; a `pid` the host cannot
read binds nothing. A report that carries `pid` and
`monotonic_sequence` for a live session whose worker speaks protocol 8 is
forwarded into the worker, which validates and journals it before the daemon
answers: `recorded: true` then means the identity is durable, and
`recorded: false` also covers a report the worker refused (a process outside the
session, a reused pid, an older sequence than an earlier claim of the process).
Any other report, and any report whose forward failed, is held in memory only
and answers as before.

`session.release_agent` accepts the same `source`/`agent` identity plus an
optional `seq`, and the same optional `pid`, `pid_start_identity` and
`monotonic_sequence`. A release that names an identity the host contradicts is
ignored (`released: false`); a release whose process has already exited is not
checked and still applies, except that a release the daemon cannot forward never
ends a claim the worker journaled (`released: false`). A release clears only the current matching active-agent claim;
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

Session metadata filters use the same opaque string keys and values as
`session.set_metadata`. `metadata_eq` matches one key to one exact value;
`metadata_has` matches when the key is present, including when its value is an
empty string. Their tagged shapes are:

```json
{"key":"metadata_eq","value":{"key":"ticket","value":"ABC-1"}}
{"key":"metadata_has","value":{"key":"ticket"}}
```

Metadata predicates combine with each other and all other session predicates
using AND semantics. The daemon applies the same key and value validation used
by `session.set_metadata`; it does not interpret or normalize either string.
They filter the sessions returned by the existing list operation without
changing its visibility or response constraints. A protocol-3 client cannot
express these filters. A current Rust or TypeScript SDK returns
`daemon/daemon_protocol_too_old` for a protocol-3 daemon before it can send a
request whose metadata predicates compatibility translation would drop.

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
  A `severity` or `status` a newer daemon defines reads as `unknown` on the Rust
  client and the CLI, so `notifications watch` keeps streaming past such a
  record; an `unknown` status matches neither `--unread` nor `--status`. The daemon never
  emits `unknown`, rejects it in params and in its notification store, and the
  hidden variant is not in the TypeScript types.
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
  source namespace. The managed Claude and Codex hooks send
  `hook:<agent>:<session id>:<provider event>:<event id>` (without the session
  id when the hook has none, and `hook:<agent>:sha256:<digest>` when the key
  would exceed 256 characters). The event id is the payload's `hook_event_id`,
  `event_id` or `id`, else a random per-invocation value that the worker route,
  the daemon fallback and every retry of that invocation share, so two sessions
  never merge and one invocation never creates a second record. The store's
  merge is unchanged; hooks installed before this key existed keep their old
  key until `pohunek service upgrade` reinstalls the integrations the new
  release lists (a manual `pohunek integration install` is needed only for
  homes the upgrade plan does not list).
- `dedupe_key`: optional source-independent id for one logical event. Session
  attention notifications use `attention:<session_id>`; session turn-completion
  notifications use `turn:<session_id>`.
- `read_at`, `acked_at`, `archived_at`, `deleted_at`: lifecycle timestamps set
  by status transitions.
- `turn_started_at`, `turn_duration_ms`, `session_metadata`: optional, on a
  `turn_completed` that the daemon stamps: the projector's, and a provider hook
  report that attaches to a turn the session's current agent runs.
  `turn_started_at` is the daemon's clock reading of the turn's first transition
  into `working`; a turn that waited in `blocked` and resumed, or went `idle`
  and worked again within the settle window, is still one turn.
  `turn_duration_ms` runs from it to the moment the session left `working`, on
  the monotonic clock; `session_metadata` is a verbatim copy of the session's
  owner-set metadata at that moment, kept after the session is removed. The
  daemon gives every turn an identity of its own and decides which turn a
  report ends: the projector's report names its turn, and a provider hook
  report (which carries no identity) attaches to the newest turn of the
  current agent that can still end. Such a report is held, and evaluated again
  every window, until its turn is over for good, so one turn is one record.
  A turn whose agent or runtime leaves while it works, that ends `blocked`, or
  whose session ends is never reported; a turn that had already gone idle when
  its agent or runtime left is reported as it ended. A hook report that
  attaches to no turn (a turn the daemon did not observe) carries none of these fields. A producer cannot send
  these fields: `notification.create` ignores them.
- `superseded_by`: optional replacement link. Older unread `turn_completed`
  records are acknowledged with `superseded_by` pointing at the newer turn or
  attention record that made them stale.

A notification store this build cannot use safely (`notifications/format.json`
names a newer format or cannot be read, an action line cannot be decoded, or
`policy.json` is rejected) does not stop the daemon. Every notification RPC then
answers the `daemon` error `notifications_unavailable` with a recovery hint (a
create with an invalid session id or reserved dedupe key reports that validation
error first), the
notification files are left byte for byte as they were, and producers neither
write nor compact until a build that reads the store runs or the `notifications`
directory is moved aside. A hook or projector turn report is refused the same way, but its turn
signal still reaches `session.wait_turn`.

`notification.list` sorts by `created_at` descending, then `id`, and omits
deleted records by default. `NotificationListParams` can filter by status, kind,
severity, provider, session id, and creation time range, plus `limit` and
`cursor`.
A result whose records do not fit one control line answers
`runtime/notification_list_too_large` instead, with a recovery hint to request
fewer records with `limit` and follow `next_cursor`; `next_cursor` is set only
when `limit` cut the result. A caller that omits `limit` gets the whole
matching set only while it fits one response, so a client that must list a
growing history pages. `pohunek notifications list` without `--limit` does so:
it follows `next_cursor` with pages of 200 records and halves the page size
when the daemon answers `notification_list_too_large`. A daemon of an earlier
release sends a line longer than the control-line limit instead, which ends
the connection; the listing then starts again from the first page at half the
size on a new connection, so no cursor crosses connections.

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
  The default policy has no overrides, and an empty map is omitted on the wire.
- `retention`: automatic daemon-owned cleanup and physical-compaction settings.
  Persisted policies without this additive field receive the defaults.

For example, a policy with one operator-set override that enables
`turn_completed` for the `pi` runtime:

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
    "pi": {
      "agent_blocked": true,
      "approval_required": true,
      "turn_completed": true,
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
by default. The daemon emits one `turn_completed` per finished turn of every
session, so enabling it (`notifications policy set`) is meant for managers and
dashboards, not for interactive inboxes; with the kind disabled, the turn is
still recorded for `session.wait_turn`. The default policy has no per-provider entries, so every provider,
whatever runtime it names, uses `enabled` until an override is set for it. A
policy file the daemon already persisted is loaded as written, including any
per-provider entries it holds; those overrides keep shadowing `enabled` for
their providers, so change them with `policy set --provider <runtime-id>` to
the wanted value.

Every retention duration and `compaction_min_actions` must be greater than zero.
The daemon runs one sweep at startup and then every `sweep_interval_secs` using
the current policy. Informational/success, warning, acknowledged attention,
acknowledged error, and archived records use their respective TTLs. Unread or
read action-required and error records are never deleted automatically. After
eligible records receive normal deletion events, the store atomically rewrites
the action log to one current action per non-deleted record once the configured
action threshold is reached. Deleted daemon-projected terminal notifications
also keep bounded, invisible replay evidence: at most one deleted snapshot per
terminal kind (`error`, `session_finished`) for each durable session's current
runtime generation. This prevents restart replay from recreating an owner-deleted
terminal notification. The next eligible compaction removes that evidence after
the source session is removed or advances generation. If session metadata is
unreadable, rejected, or has a torn final record, compaction keeps every terminal
tombstone rather than guessing which sources are absent, while still compacting
other action history. It logs this condition once per store instance; the normal
per-generation bound resumes once the metadata can be read completely.

Policy is enforced daemon-side for all notification producers. If a producer
creates a disabled kind, `notification.create` returns
`runtime/notification_kind_disabled`.

A source ranks as a provider hook exactly when its `provider` is an identity
provider of a registered hook schema (today `shell`, `codex`, `claude`, and
`hermes`, matched case-sensitively). These are the providers whose
session-scoped `notification.create` reports are checked against the session's
hook schema; that admission additionally admits a session's own runtime, which
does not change its rank. `pohunek` and `daemon` rank as daemon projectors.
Every other producer, including a runtime that no hook schema lists, ranks as a
user/external source.

Provider hooks have higher source priority than daemon projectors for the same
`attention:<session_id>` key inside `attention_dedupe_window_secs`. Every
`notification.create` on an `attention:<session_id>` key is deferred, so it
answers `created: true` with a minted id; the dedupe happens when the debounce
flush commits the record. A provider hook record then upgrades an existing
projector attention record in place and emits `notification_updated`. A
projector record for a key that already has a provider-backed record is
dropped. Provider hooks of equal rank dedupe against each other: a `hermes` or
`shell` record for a key that already has a provider-backed record inside the
window (from the same or another hook provider) is dropped. A dropped record
never persists and emits no event, so its minted id is never listed,
`notification.update` on it returns `notification_not_found`, and
`notification.delete` returns `deleted: false`.
User/external sources do not automatically supersede provider records. While
records for one session key are held pending in the debounce window, the
higher-ranked record is kept and an equal-ranked later record replaces the
earlier one, so a provider hook report pending beside a user report is the one
committed.

`turn:<session_id>` has different semantics: it is not time-window deduped. A
new unread `turn_completed` for the same session acknowledges any older unread
turn immediately and sets the older record's `superseded_by` to the newer id;
each turn keeps its own record, and the record of a finished turn is
acknowledged when the next turn starts. Reports of one turn are merged while
pending and add nothing once the turn was reported; reports of different turns
are never merged. A provider report that names no turn right after the
session's turn reached the inbox, within that repeat window, does not add a second inbox record.
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
emitted for a pending record. A `turn_completed` record is stamped with
`turn_started_at`, `turn_duration_ms` and `session_metadata` when it flushes, so
the record in the `notification.create` result does not carry them; the one in
`notification.list` and the events does.

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

`integration.install` runs the Codex and Claude integration handlers, which
install durable state and notification hooks for current Codex and Claude
builds only. There is no fallback for older provider hook APIs.

Both integration handlers also register the state hook for `SubagentStart` and
`SubagentStop`. These callbacks target only the owner-private worker endpoint; when
it is unavailable or refuses the claim they exit 0 with no output and record
the failure in the hook failure trace (see "Hook failure trace" below). They copy only
lifecycle identifiers and agent type. A Claude `SubagentStop` with no recorded
start is not a subagent and adds no row, while a Codex one records a completed
child; see `subagents` under `SessionInfo`. The Claude integration handler also
registers the state hook on the main-agent `Stop` (action `stop`): it sends the worker one
`subagent_snapshot` claim with, for each entry of `background_tasks`, the
required `id`, `type` and `status`, and the optional fields that sanitize:
`label` from `description` (every type; control and format characters removed,
whitespace collapsed, cut to the byte limit), `program` from `command` (shell
tasks only; the basename of the command word, after skipped `NAME=value`
assignments, `cd <dir> &&` or `;`, and `env` options, accepted only when it
matches `[A-Za-z0-9][A-Za-z0-9._+-]*`; a command longer than 64 KiB gets no
program, because shell tokenizing grows superlinearly with token length, about
32 ms at 64 KiB and 6 s at 1 MiB, and a cut command word could name another
program), and `agent_type` (subagent tasks only; a value with
a control, format, line or paragraph separator or surrogate character is
dropped). An invalid optional field is omitted from its task alone. The command
and its arguments are never forwarded. The label and program byte limits come from the
worker-injected reserved variables `POHUNEK_BACKGROUND_LABEL_MAX_BYTES` and
`POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES`; when one is missing or invalid, the hook
sends no label or program at all. The hook sends nothing for a `Stop` with a
non-empty `agent_id`, a missing or invalid list, a list over 256 entries, or
input over 1 MiB, and a missing or invalid required field voids the list. A
worker that predates the claim refuses it and nothing is applied. Codex `Stop`
carries no task list, so the Codex integration handler registers no state hook
on `Stop`. The hook
validates its action and Pohunek handshake before reading, rejects oversized
JSON, and never stages the payload on disk; provider prompts, results,
messages, and transcript paths are discarded.

The Claude integration handler also registers the state hook for
`UserPromptSubmit` (action `prompt-submit`). Claude runs that hook before every
prompt, injects its standard output into the model context, and blocks the
prompt until it exits, so the action reads the prompt input in fixed-size chunks
and discards it without decoding (a prompt over 64 KiB still reports), writes
nothing to standard output or standard error, exits 0 on every path, and makes
one attempt at the worker socket with no retry and no daemon fallback. The
report is the worker's `prompt_accepted` request: the provider, the process id
and start identity of the agent, the hook's `CLOCK_MONOTONIC` start time as its
sequence, and a short expiry. The worker applies it only from the process its
journal names as the session's launch identity (a nested `claude -p` and a
`launch_identity_pending` session are refused), only inside a Claude session
(Codex, other providers and a shell-hosted Claude are refused), and only with a
sequence above the last applied one. It keeps the sequence in memory and shows
it as `prompt_accepted_sequence` in the inspect snapshot of a connection that
negotiated worker protocol 8. A worker that predates the request refuses it and
the hook exits 0 with no output. The report is delivery evidence for a waited
`session.input`; it never changes activity, the activity revision, events or
turns.

The report proves only that Claude started its `UserPromptSubmit` hooks for a
prompt. Claude runs the hooks of one event in parallel, so another hook of the
user that blocks the prompt (exit code 2 or `decision: block`) can still
discard it after this hook reported; the wait then succeeds on an acceptance
while the prompt never reaches the model. A turn the agent starts on its own
between the Enter write and the agent reading Enter has the same effect. Both
are known limits of delivery evidence: callers that need to know the agent
worked on the prompt wait for `working` activity.

On `SessionStart` (action `session`) the hook's identity report carries the
hook input `source` as `session_source` only when it is `startup`, `resume`,
`clear` or `compact`; any other value, or none, omits the key. The worker clears
the background snapshot only on `startup` and `resume`. `fork` is not forwarded:
a forked session is a new process and must not clear the snapshot of the session
it was forked from. On `SessionEnd` (action `release`) the identity release
carries the hook input `reason` as `release_reason` only when it is `clear`,
`resume`, `logout`, `prompt_input_exit` or `other`; any other value omits the
key. The snapshot belongs to the reporting Claude process: any process in the
managed PTY tree may report, the latest turn end wins, the owner's release with
a reason other than `clear` clears it, and a `SessionStart` from another process
clears it. A `/clear` keeps the snapshot, because Claude follows that release
with a `SessionStart` of the same process.

The four integration methods classify an explicit `agent` before acting: a
value outside the runtime-id grammar is `runtime/agent_kind_unsupported`, a
valid id no enabled runtime backs is `runtime/runtime_not_installed`, and an
installed runtime with no daemon-managed hook integration (the shell, Hermes,
or any other registered runtime) is `runtime/agent_not_installable`.

Install, status, doctor, and uninstall dispatch through the integration handler
the runtime definition names (`integration.handler`), never through the runtime
id. The handlers are compiled core code and a closed set (`codex-hook-v1`,
`claude-hook-v1`, and `hermes-hook-v1`, whose lifecycle the CLI runs, so the
daemon answers `agent_not_installable` for it). A handler owns one active asset
set, its accepted paths and modes, conflict detection, and rollback; a package
never supplies installer logic. Bare `integration.install`, `status`, and
`doctor` visit each distinct config home of the daemon-run runtimes once (the
same handler on the same canonical directory is one home), in runtime-id order
of the first runtime naming it, and reports carry the runtime the request
addressed.

#### Hook failure trace

The Codex and Claude state hooks never break the provider: every path exits 0
and writes nothing to the provider. Once a state hook has established that it
runs inside a Pohunek session (`POHUNEK_ENV=1` and a non-empty
`POHUNEK_SESSION_ID`) and has started its Python body, every reason it fails to
deliver a report on a route is recorded as one line in a hook failure trace, so
a lost claim, snapshot or identity report leaves evidence. An exit before that
point writes no line; in particular a hook without `python3` on its `PATH`
writes no trace, because a POSIX shell cannot open the trace without following
a symlink.

The worker sets two reserved variables on every managed child (see
Active-Agent Hook Payloads): `POHUNEK_HOOK_TRACE_PATH`, the absolute path of
`pohunek-hooks.jsonl` in the pohunek log directory (by default
`~/.local/state/pohunek/logs/`), one file shared by every session of the host,
and `POHUNEK_HOOK_TRACE_MAX_BYTES`, the size at which the hooks rotate it
(1 MiB). A hook traces only when the path is absolute and the size is a
positive decimal integer of at most 15 digits without a leading zero;
otherwise it writes no trace and still exits 0.

Each line is one JSON object of at most 512 bytes; a line that would be longer
is dropped, never truncated. Fields:

- `ts`: the time of the line, RFC 3339 in UTC with milliseconds;
- `session`: the Pohunek session id from `POHUNEK_SESSION_ID`;
- `provider`: the runtime id the hook asset was rendered for, such as `codex`
  or `claude`;
- `action`: the hook action (`session`, `release`, `subagent-start`,
  `subagent-stop`, or `stop` and `prompt-submit` for Claude);
- `reason`: a stable snake_case code, listed below;
- `detail` (optional): a short fixed code, an errno name such as `ECONNREFUSED`
  or one of the codes listed below, never text taken from the input or an
  error message.

Reason codes, with their `detail` values:

- `input_too_large`: the provider input exceeded the hook's read bound
  (64 KiB, 1 MiB for the Claude `stop` action);
- `input_invalid`: the input could not be used; `detail` is `empty`,
  `not_json`, `not_utf8`, `too_deep`, `not_object`, an errno name when reading
  it failed, `agent_id_missing` (a subagent action without a non-empty
  `agent_id`; the Muse reporter names it `subagent_id_missing`) or `session_id_missing` (a `session` action without a native
  session id);
- `no_snapshot`: the Claude `stop` action sends no snapshot; `detail` is
  `subagent_stop` (a `Stop` with a non-empty `agent_id`), `tasks_missing` (no
  `background_tasks`), `tasks_invalid` (not a list, or an entry without a
  non-empty string `id`, `type` or `status`) or `tasks_over_bound` (more than
  256 entries);
- `action_invalid`: a defensive code for an action the Python body does not
  handle; the managed entry point exits on an unknown action before the session
  handshake, so it never writes this line;
- `reminder_thread`, `thread_not_root`, `source_unknown`, `muse_session_missing`:
  the Muse reporter dropped a report: a `-reminder` session or subagent id, a
  `SessionStart` whose `session_id` is not `MUSE_SESSION_ID`, a `source` that
  is not `startup`, `resume`, `clear`, `compact` or `fork`, or a hook run
  without `MUSE_SESSION_ID`;
- `worker_socket_missing`: `detail` `unset` when `POHUNEK_WORKER_SOCKET_PATH`
  is not set for a report that needs the worker, or `ENOENT` when no socket
  exists at that path;
- `worker_instance_missing`: `POHUNEK_WORKER_INSTANCE_ID` is not set;
- `agent_pid_invalid`: the hook could not determine the agent's process id;
- `agent_identity_unavailable`: the start identity of the agent process could
  not be read;
- `worker_unreachable`: connecting to the worker socket failed (`detail` is
  the errno name);
- `worker_timeout`: the worker did not accept or answer within the 0.5 s socket
  timeout;
- `worker_closed`: no usable answer line; `detail` is an errno name, `empty`
  or `malformed`;
- `worker_refused`: the worker answered without `ok: true`;
- `prompt_report_failed`: the Claude `prompt-submit` action raised an
  unexpected error while building or sending its report; the hook still exits 0
  with no output, so the prompt is never delayed or altered;
- `daemon_socket_missing`, `daemon_unreachable`, `daemon_timeout`,
  `daemon_closed`: the same failures on the daemon-socket fallback that the
  `session` action (and the Claude `release` action) uses;
- `daemon_refused`: the daemon answered with an error other than a retryable one;
- `daemon_retry`: the daemon answered `hook_schema_unavailable` and the hook
  tries the report again; `daemon_retry_exhausted`: the retry deadline ended
  first and the report is dropped;
- `daemon_protocol_invalid`: no worker socket is set and
  `POHUNEK_PROTOCOL_VERSION` is not a usable integer, so the daemon fallback
  cannot run.

A line never carries provider payload content, ids taken from the input
(agent, task or native session ids), task data, paths, or environment values;
the only identifier is the Pohunek session id. One hook run can write more than
one line: a `session` or `release` report that the worker does not accept and
the daemon fallback then delivers keeps the worker's line, and a run whose
first route succeeds writes none.

The hook queues its lines and writes them when it exits, after every exchange
with the worker or the daemon, so tracing adds no delay to a report. It opens
the trace with `O_APPEND`, `O_CREAT`, `O_NOFOLLOW` and `O_NONBLOCK` and mode
`0600`, so a symlink at the path is never followed and a new file is
owner-private, and takes an exclusive lock on the opened file for each line
without blocking: it tries the lock at most 5 times, 5 ms apart, and when the
lock stays contended it drops the run's remaining lines, so a held lock never
stalls the hook. It writes nothing unless the file is a regular file owned by
the current user, with a single link and no group or other permission bits. Each line is one
`write`, so lines of concurrent hooks never interleave. When a line would take
the file past `POHUNEK_HOOK_TRACE_MAX_BYTES`, the hook renames it to
`pohunek-hooks.jsonl.1` (replacing an earlier rotated file) and writes to a new
`pohunek-hooks.jsonl`, so the trace holds at most twice the bound across the
two files. A failure to write the trace is ignored: tracing never changes the hook's exit code or output.

#### Config homes and host profiles

The directory a handler installs into is the config home its runtime declares
(the `[config_home]` descriptor table, see "Config home" under the runtime
package format), resolved the way a launch resolves it: the declared variable
read from the environment the launched agent sees (the daemon's base
environment, which only forwards variables its allowlist names, overridden by
the host profile's own `[env]`), else the declared default below that
environment's `HOME`. A value that is not an absolute path (a literal `~`
included) is `agent_config_dir_invalid`; it is never expanded with the daemon's
`HOME`. A runtime that names a daemon-run handler but declares no
`[config_home]` is `agent_config_home_undeclared`. The official Claude Code and
Codex packages declare `CLAUDE_CONFIG_DIR` / `.claude` and `CODEX_HOME` /
`.codex`.
The daemon's own process environment steers nothing unless the allowlist
forwards the variable, so an install, a status report and a launched agent
always agree on the directory. A daemon that relied on such a variable in its
service environment must add it to the environment allowlist (so launched agents
see it too) or move it into a host profile's `[env]` and select the profile;
hooks installed in the directory the daemon's own variable names are not
reported. A package runtime without `[config_home]` is left out of bare
`install`, `status` and `doctor`.

`profile` (a host profile name) and `all_profiles` (boolean, exclusive with
`profile`) on `integration.install`, `status`, `doctor` and `uninstall` select
which home a request acts on; without either the runtime's own (profile-less)
home is used and the request is unchanged. `profile` must be a host profile of
the selected runtime (`integration_profile_runtime_mismatch` otherwise; a name
that is no profile file, a bare runtime name included, is
`agent_profile_not_found`), and without `agent` the profile's runtime is the
selection. `all_profiles` covers the runtime's own home and the home of each of
its host profiles. A profile over the runtime that does not resolve (unpinned
over a package-served alias, pinned to a version that is not installed, or over
an alias nothing serves) is still covered: `status`, `doctor` and `uninstall`
address the home its own `[env]` names, read with the same containment and
strict parse as a load, and `install` lists it in `failed` with the profile's
resolution error. A file that cannot be parsed names no runtime and is skipped
and logged, as `host.inspect` does. Selectors that resolve to the same canonical
directory share one home and one transaction; the report's `home`
(`{profiles, bare, selector}`) names every selector it stands for and carries
`selector` (`{kind: "default"}` or `{kind: "profile", name}`), the one the
daemon verified reaches the directory the installer uses: an alias can resolve
through a symlink the installer refuses, so recovery commands are built from
`selector`, never from the order of `profiles` or from `bare`. A request without
`agent` that reaches one directory through one handler for several runtimes (a
package that declares `claude-hook-v1` and Claude's `[config_home]`, say)
attributes the directory to none of them: each of those runtimes keeps one home
whose resolution fails with `configuration/integration_home_ambiguous_owner`, so
a bare `install` fails with it before writing any home (under `all_profiles` it
is listed in `failed`). `status` reports the home as unresolved with a warning
that carries the code and its hint; `integration.doctor` reports it as a
`config_root_invalid` finding whose remediation names `--agent <runtime>` or
separate config homes, and `daemon.doctor` reports that home's
`agent_integration_<runtime>` check as `warn`, not as malformed state. The hint
names `--agent <runtime>`; a request that names `agent` addresses only that
runtime and is not refused.
`IntegrationStatusResult` and `IntegrationDoctorResult` carry
`home_selectors: true`; a daemon that predates the selectors omits it (and an
older `integration.install` silently ignores the selector fields), so a client
checks the marker before it sends a mutation that carries a selector. Install under
`all_profiles` skips homes whose directory does not exist and fails with
`agent_config_dir_missing` only when none exists. Each home is its own
transaction under its own installer lock, so a failing home rolls back to its
exact prior tree and leaves the others as they are; `install` and `uninstall`
list a failing home in `failed` and the CLI then exits non-zero, and `status`
and `doctor` report an unresolvable home as a degraded report instead.

`profile` and `all_profiles` are accepted only from the local control socket:
reports, errors and installed paths name directories derived from a profile's
environment, which can hold secrets. A remote overlay connection gets
`daemon/local_only_method` before any parameter is acted on. The team relay is
not a daemon transport yet; a relay-originated path must never arrive as local.

An install over an existing asset set is an update transaction: the handler
stages the new set against the active one without changing it, proves
compatibility, activates atomically, and restores the exact prior tree when any
step fails, so the active set stays in place until activation succeeds.
Compatibility requires the runtime's hook schema to be driven by the handler and
to admit every operation the new set reports, as derived from the staged
registration (the commands that run the managed scripts, restricted to the
arguments each embedded script accepts) and not from a declaration, and every hook schema of the
package versions that live, lost or resumable sessions and host profile pins
still reference for that handler to admit them too (an undeterminable retained
version, whose handler is unknown, refuses an update of every handler's asset set; while a `metadata.jsonl` line that may pin a package cannot be read, every installed version counts as retained, and the refusal is logged with a pointer to the unreadable line); otherwise the update is refused
with `runtime/integration_update_incompatible` before anything is written. An
active set of any version is replaceable, so a rolled-back release reinstalls
its older set.

`integration.status` is the corresponding read-only drift report. Bare status
reports every served runtime whose daemon-run handler has a config home (the
Codex and Claude Code packages once installed); `--agent <runtime>` selects
one. `--agent` takes any runtime id: the CLI sends it unchanged and the daemon
resolves it to the handler the runtime definition names (an id no installed
runtime backs is `runtime_not_installed`, a runtime without a daemon-run handler
is `agent_not_installable`), so a package runtime is addressed by its own id and
the recovery commands in status and doctor output name that id. `hermes` keeps
its local CLI-run lifecycle. Every managed script is checked independently against its embedded asset
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
`elicitation_complete`, and `elicitation_response` create `system`. The
notification hook's `Stop` creates `turn_completed`; `StopFailure` creates
`error`. The separate state hook entry on `Stop` creates no notification; it
sends only the subagent snapshot to the worker and has no daemon fallback.

When `POHUNEK_SESSION_ID` is valid, the managed hooks add
`attention:<session_id>` to attention events and `turn:<session_id>` to
`Stop`/`turn_completed` events. Invalid session ids are dropped before either
`session_id` or `dedupe_key` reaches the daemon.

The managed hooks read at most 64 KiB from provider stdin (1 MiB for the Claude
state hook's main-agent `Stop`, whose input carries the final assistant message
and the task list), validate action and
environment before reading input, silently drop an invalid `POHUNEK_SESSION_ID`,
and exit successfully without output on local failures so agent sessions are not
disrupted; the state hooks also record each such failure inside a Pohunek
session in the hook failure trace (see "Hook failure trace"). Reinstalling hooks removes only exact command shapes managed by
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
| `details` | object | no | Typed facts a caller reads instead of parsing `msg`. Only `command_failed` carries it: `{process_exit: {exit_code?, signal?}}`, exactly one of the two set. Decoders ignore members and kinds they do not know. No other error has the key, and only a client that called `command.run` can receive it, so older clients (whose strict decoder rejects unknown error fields) never see it. |

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
| `configuration` | `paths_unavailable`, `overlay_configuration_invalid`, `overlay_registry_invalid`, `invalid_discovery_options`, `agent_config_dir_invalid`, `integration_path_untrusted`, `integration_trust_conflict`, `integration_home_ambiguous_owner`, `session_new_secret_env_relay_refused` |
| `daemon` | `version_mismatch`, `native_store_check_task_panicked`, `version_adapter_failed`, `daemon_protocol_too_old`, `version_translation_failed`, `method_not_found`, `bad_request`, `daemon_unreachable`, `remote_daemon_unavailable`, `host_governance_unavailable`, `session_input_wait_contract_mismatch`, `projects_not_configured`, `serialize_failed`, `json_error`, `project_task_panicked`, `doctor_task_panicked`, `assistant_materialize_task_panicked`, `assistant_method_unsupported`, `attach_self_feedback`, `daemon_shutting_down`, `hook_schema_unavailable`, `session_new_secret_env_unsupported`, `command_unsupported`, `package_install_commands_unsupported`, `notifications_unavailable` |
| `discovery` | `<overlay>_cli_missing`, `<overlay>_state_unavailable`, `<overlay>_listener_address_missing`, `overlay_discovery_failed`, `overlay_peer_collision`, `overlay_host_ambiguous`, `overlay_host_unavailable`, `overlay_error`, `host_unknown`, `host_identity_unpinnable`, `remote_discovery_failed` |
| `runtime` | `agent_binary_missing`, `agent_profile_not_found`, `invalid_profile`, `secret_env_not_declared`, `secret_env_missing`, `secret_env_invalid`, `agent_not_resumable`, `agent_native_reference_missing`, `agent_native_store_relocated`, `native_identity_missing`, `native_identity_unverified`, `native_identity_uncertain`, `native_identity_evidence_unavailable`, `native_report_unattested`, `not_resumable`, `invalid_session_ref`, `no_capable_agent`, `integration_no_managed_runtime`, `bundle_unavailable`,`assistant_bundle_mismatch`, `materialization_failed`, `agent_cannot_read_bundle`, `session_not_found`, `session_not_running`, `session_not_terminal`, `session_external_read_only`, `session_exit_timeout`, `session_runtime_commit_stale`, `session_runtime_conflict`, `session_runtime_reconnecting`, `runtime_supervision_unavailable`, `runtime_supervision_ambiguous`, `runtime_identity_mismatch`, `migration_manifest_missing`, `attach_not_found`, `attach_expired`, `worker_attach_stream_failed`, `worker_protocol_incompatible`, `worker_controller_busy`, `worker_identity_mismatch`, `worker_invalid_state`, `worker_invalid_request`, `worker_invalid_data_token`, `worker_write_outcome_unknown`, `worker_runtime_fault`, `session_input_stalled`, `session_cwd_missing`, `session_tmpdir_unsafe`, `session_tmpdir_failed`, `client_file_descriptors_exhausted`, `system_file_descriptors_exhausted`, `pty_alloc_failed`, `spawn_failed`, `pty_error`, `io_error`, `project_store_error`, `project_detect_failed`, `not_a_git_repo`, `project_not_found`, `project_ambiguous`, `prompt_not_found`, `template_not_found`, `action_not_found`, `invalid_name`, `invalid_template`, `invalid_action`, `path_escape`, `config_read_failed`, `agent_not_installable`, `agent_config_dir_missing`, `integration_settings_invalid`, `integration_io_failed`, `worktree_store_error`, `worktree_path_conflict`, `invalid_base_branch`, `base_branch_fetch_failed`, `worktree_branch_in_use`, `worktree_add_failed`, `hook_required_failed`, `branch_fetch_failed`, `remote_branch_not_found`, `branch_diverged`, `backup_ref_exists`, `invalid_branch`, `invalid_branch_slug`, `notifications_not_configured`, `notification_task_panicked`, `notification_store_error`, `notification_not_found`, `invalid_notification_transition`, `invalid_notification_metadata`, `invalid_notification_session_id`, `invalid_notification_dedupe_key`, `notification_kind_disabled`, `invalid_notification_timestamp`, `invalid_notification_cursor`, `invalid_notification_policy`, `integration_install_in_progress`, `integration_destination_collision`, `integration_recovery_required`, `integration_update_incompatible`, `launch_input_invalid`, `launch_input_unavailable`, `integration_overlay_unavailable`, `integration_launch_arg_conflict`, `command_package_not_installed`, `command_not_selected`, `command_not_declared`, `command_not_exposed`, `command_package_disabled`, `command_busy`, `command_failed`, `command_timed_out`, `command_output_too_large`, `command_output_not_utf8`, `command_unavailable` |

In the `discovery` row `<overlay>` is `netbird` or `tailscale`: `netbird_cli_missing`,
`netbird_state_unavailable` and `netbird_listener_address_missing` come from the
NetBird provider, and `tailscale_cli_missing`, `tailscale_state_unavailable` and
`tailscale_listener_address_missing` from the Tailscale provider. A malformed
`overlays.toml` is always `overlay_configuration_invalid`; no provider has its own
configuration code.

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
`session_input_not_submitted`, `session_input_timeout`, `session_input_stalled`, `session_input_not_ready`, and the CLI-local `session_input_interrupted`. Daemon startup may additionally return
`observation_limits_invalid`. `runtime_not_installed` is emitted when a runtime
id resolves to no enabled runtime: a host profile whose `base` names an
uninstalled runtime, and resume, fork or mutation of a session whose runtime is
not installed or no longer matches its recorded launch binding. A bare
`session.new` agent name that is neither a profile nor an installed runtime
stays `agent_profile_not_found`. For an official alias nothing serves (`codex`,
`claude` or `hermes` without its package), that `agent_profile_not_found` and the
`runtime_not_installed` of an unpinned host profile over the alias carry a
recovery hint naming the catalog install of the official package. While the
installed official package of that alias (`pohunek.runtime.<alias>`) cannot be
loaded (after a registry rebuild left it out), that unpinned profile's launch is `runtime_incompatible` instead, with
the recovery `pohunek plugin doctor` and then `pohunek plugin profile migrate
<name>`; the install is not offered, because the same archive is refused.
`runtime_builtin_removed` is emitted by resume, fork and mutation of a session
launched from the built-in runtime of an official alias this daemon does not
compile in while no package serves the alias. Once the official package serves
it, the package adopts the session, and resume and fork answer
`runtime_served_by_package` only when the recovery contract is not honored
(fork of a Hermes session: `agent_fork_unsupported`). `runtime_incompatible` is emitted when a
runtime package is installed but its root is missing, modified or contradicts
the session's pin, for fresh launch, resume, fork and integration changes (the
message names only the runtime id). `agent_profile_revision_stale` and `agent_profile_revision_invalid` are emitted only by the relay-selected launch
source (see above), which no wire method reaches yet.
`agent_profile_revision_unavailable` is also emitted by `session.new` for a profile-backed session, and by `session.resume` and `session.fork`, when the host's revision key cannot be read.
`agent_profile_changed`, `agent_profile_missing` and the daemon-class `agent_profile_change_local_only` are emitted by `session.resume` and `session.fork` (see "Frozen profile revision").
Observation request errors intentionally carry no terminal
payload or current-runtime payload; refresh `session.inspect` or restart
observation from a fresh screen/tail when recovery requires new coordinates.

`session_input_wait_contract_mismatch` means the SDK cannot prove whether a
same-version daemon honored the wait contract. Delivery outcome is unknown: do
not retry blindly. Upgrade daemon and client together, inspect the session, and
only resend when the observed terminal state proves the original input was not
applied.

`session_input_timeout` also treats delivery as potentially unknown. Inspect the
current session before deciding whether to resend, and do not retry blindly. On
delayed-submit framing it also covers the case where Enter was written and no
requested activity followed it.

`session_input_not_submitted` means a waited input's body may sit in the agent's
input box while Enter was not written: the agent turned `blocked` between body
and Enter, or the deadline or a daemon shutdown arrived first. Typed text cannot
be retracted from an arbitrary TUI, so inspect the session and answer or clear
the prompt before sending new input. `session_input_wait_unsupported` is
returned only by a daemon that cannot pace delayed-submit input, before any byte
is written; this daemon never returns it, and clients treat it as a refusal.

`session_input_stalled` means the PTY accepted no input for the configured stall
bound, so the agent is not reading its terminal. Part of the plan may already
have been written and the rest was not. A `session.new` with initial input
removes the session it was creating, so there is nothing to inspect there; a
`session.input` leaves the session running and the caller inspects it before
resending. The submit is never sent after a stall, but for an agent with
bracketed-paste framing the accepted part can leave a paste open in its input,
so a resend would land inside it: clear the agent's input (attach and use the
agent's own clear or cancel key) before sending again. Attached terminal input
is not bounded; it blocks like a real terminal until the agent reads.

`session_input_not_ready` from `session.new` is definite: the initial input was
not written and the session was not kept, so the create can be retried once the
agent can accept input. The message names the grace and the last observed
activity; a `blocked` activity means the agent waits on a prompt that needs an
operator answer.

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
| `session_runtime_lost` | `{session: SessionInfo}` | The worker or host runtime is gone. The logical record remains visible and may support explicit recovery. When the daemon observed the runtime end, `session.state` is `failed` with no `activity`; otherwise the session keeps its state and has no `activity`. `session.runtime.exit` carries the service manager's verdict on the worker job when it held one. |
| `session_runtime_conflict` | `{session: SessionInfo}` | Runtime discovery found duplicate, mismatched, or otherwise ambiguous live identity. The daemon quarantines the conflict and does not kill a worker automatically; it re-checks a conflict whose record names a worker generation in the background, and `session.stop` can end one whose recorded identity the journal proves. |
| `session_runtime_discovered` | `{entry: RuntimeInventoryEntry}` | Startup reconciliation classified a discovered durable worker that is not a plainly managed runtime (orphaned, conflicting, incompatible, or identity-mismatched). Emitted once per non-managed discovery so operators can inspect quarantined runtimes. |
| `session_native_recovered` | `{session: SessionInfo, previous_worker_instance_id?: string, worker_instance_id?: string}` | Explicit provider-native recovery created a new worker and runtime generation for the same logical session. `previous_worker_instance_id` can be absent for a one-time migrated legacy session; production worker recovery includes the new `worker_instance_id`. |
| `agent_state` | `{session_id: SessionId, activity: AgentActivity, source: StateSource, runtime?: SessionRuntimeIdentity, activity_epoch?: string, revision?: ActivityRevision}` | Agent activity changed. The daemon emits it only when the activity or its source changes: an idle session produces no further `agent_state` events and its `revision` stays fixed. While a `session.input` request with a `wait` is waiting on a session, the daemon also restates that session's unchanged activity as a new revision about every 800 ms, so the wait sees evidence newer than its boundary. `session.wait` and `session.output` read current state and do not trigger it. `source` may be `report` when a hook report supplied explicit active-agent state. Current daemons emit `runtime`, `activity_epoch`, and decimal-string `revision`, making `(activity_epoch, runtime, revision)` exact reconnect-safe evidence rather than a hint to re-read only the latest snapshot; the fields remain additive for general v2 subscribers, while input-wait success requires them through `SessionInputResult`. |
| `subagent_state` | `{session_id: SessionId, subagent: SubagentInfo, runtime?: SessionRuntimeIdentity}` | One provider-managed subagent changed. Current daemons emit `runtime`; clients ignore an event without a runtime identity or whose runtime id/generation does not match the session snapshot, then apply only a newer decimal-string `subagent.revision` for the same provider/id. A newer revision may move a terminal subagent back to `running`, so a terminal lifecycle is not final. `session.list` and `session.inspect` remain the reconnect seed through `SessionInfo.subagents`. |
| `background_state` | `{session_id: SessionId, background: SessionBackground \| null, runtime?: SessionRuntimeIdentity}` | The session's background-task snapshot changed. `background: null` clears it: the runtime ended, or the reporting Claude process exited, released its session (except for `/clear`) or started or resumed one, or another agent process started a session. Current daemons emit `runtime`; clients accept the event only when its runtime id and generation match the current session snapshot, the same rule as `subagent_state`. `session.list` and `session.inspect` seed the snapshot through `SessionInfo.background` after reconnect. This event is withheld from protocol 3 subscribers. |
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
for that creation request; zero is rejected. Without that flag, every creation
uses the Rust SDK's dedicated creation budget described below, which covers
the daemon's bounded worker startup and create hooks. `session new` sends one
`session.new` request and never resends it. Only its own refusals before that
request (`cli_usage`, `confirmation_required`, `confirmation_declined`) prove
that nothing was created. Any other failure leaves the outcome unknown:
`request_timeout`, whose recovery hint says so; a transport error such as
`framing` locally or `remote_daemon_unavailable` for a remote host, which may
be a reply lost after the request was sent; or a typed daemon error returned
after the session was recorded, such as `session_runtime_reconnecting`. A
create in progress is listed only once its target, the `pre-create` and
`post-create` hooks included, and its launch are done. To find the session,
pass a caller key such as `--meta launch=<id>` and look it up as described in
[Creation outcome](#creation-outcome): the lookup, how to read its result, and
why a retry can create a second session apply to `session new` as well.

Initial input waits for an editable prompt or bracketed-paste enable signal,
up to the bounded startup grace for a silent agent; the body and submit key
are sent as separate PTY writes under the runtime input rules. A runtime that
declares `ready = "screen_idle"` waits for the editable prompt alone and fails
with `session_input_not_ready` when none shows before the grace ends. A
runtime that declares `ready = "manifest_idle"` (Codex) is as strict, but
accepts the detection manifest's winning idle evidence from the screen or the
terminal title. Recognized visible blockers outrank idle titles: the Codex
package covers the update popup, login/API-key entry, incompatible settings
and folder trust (including captured 20-column wrapping). After a recognized
blocker, cached idle evidence cannot release input until fresh idle evidence
arrives after the blocker disappears. This policy does not require a positive
initialized-composer screen; a title alone can qualify before any recognized
blocker appears.
Current SDK and CLI clients set `extended_input_ready_wait` on a typed
`session.new` with initial input. The daemon then uses the configured host
startup grace, or the runtime's longer `[input] startup_grace_ms` when it
declares one (the Hermes package declares 30000 ms). A request that omits the optional field keeps its silent-reader
wait capped at 500 ms, within the previous release's five-second client
request deadline; a visible readiness signal still ends either wait at once.

For example, keep untrusted prompt text out of argv by writing it on stdin:

```bash
printf '%s' 'Redacted input.' | pohunek session input s-42 --stdin --json
```

The resulting stdout remains exactly one JSON envelope. The input bytes do not
appear in that envelope, diagnostics, or structured logs.

### Assistant request and creation outcome

`pohunek assistant` takes its free-form request either as positional words or
through `--request-stdin`, never both. With `--request-stdin` the CLI reads
bounded UTF-8 text from stdin, under the same limits and control-character rules
as `session new --input-stdin`. It trims surrounding whitespace and embeds the
result in the opening prompt exactly where the positional words go, so both forms
create the same session and print the same `--json` result. Empty input, input
that is empty after trimming, and invalid input fail with `cli_usage` before the
CLI starts or dials any daemon. A stdin request cannot also answer the remote
confirmation prompt, so a remote launch with `--request-stdin` needs `--yes`
(`confirmation_required`), as a `--json` launch does.

`--request-timeout-ms <u32>` overrides the response deadline of the creation
request only, with the same parser and meaning as on `session new`. Without it,
the creation uses the Rust SDK's dedicated initial-input budget. `--meta
key=value` (repeatable) writes caller metadata atomically with the session, as
on `session new`.

#### Creation outcome

A launch sends exactly one `session.new` request, and the CLI never resends it.
Only the CLI's own refusals before that request prove that no session was
created: `cli_usage` (including an invalid stdin request),
`confirmation_required`, `no_capable_agent`, `materialization_failed` and
`agent_cannot_read_bundle`.

Any other failure leaves the outcome unknown, because the caller cannot tell
whether it came before or after the request:

- `request_timeout`.
- A transport error, such as `remote_daemon_unavailable`, `host_unreachable`
  or a framing or I/O error. The same code reports both a connection that
  never opened and a reply lost after the request was sent.
- A typed daemon error for `session.new`. Most of them mean the create failed
  and was rolled back, as described for `session.new` above. Some are returned
  after the session was recorded, for example `session_runtime_reconnecting`.

`session.new` has no idempotency key, so pohunek cannot make a retry safe. A
retry while the first create is still running creates a second session. A
caller-chosen correlation key makes every attempt visible. Pass one, for
example `--meta launch=<unique id>`, and keep it for every attempt of the same
launch. The key name is the caller's convention: pohunek reserves and
interprets none. After an unknown outcome, look the key up before deciding:

```bash
printf '%s' 'Why does attach fail on my laptop?' \
  | pohunek assistant --request-stdin --meta launch=7f3a9c --json
# After an unknown outcome:
pohunek session list --meta-eq launch=7f3a9c --json
```

A protocol-3 host keeps the metadata but refuses metadata filters with
`daemon_protocol_too_old` before the request is sent. On such a host, list
without the filter (`pohunek session list --json`) and match each session's
`metadata` instead.

How to read the result:

- **A listed session** is that launch. It may still be starting: the record is
  written after the launch and its pre-create and post-create hooks finish, but
  before the opening prompt is delivered. It can still be rolled back by
  `session_input_not_ready` or `session_input_stalled`.
- **An empty list** is not proof that nothing was created. A create still in
  progress is listed only once its launch and hooks finish, which can take
  many minutes.
- **After a retry**, more than one session under the key means duplicates. The
  caller keeps one and stops the rest.

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

## Workflow Plugin CLI

`pohunek workflow` is a local CLI lifecycle, not a daemon public method. There
is no protocol method, event, record or SDK type for workflow plugins: the CLI
keeps them in its own package registry under `<state>/pohunek/workflow-plugins/`,
with per-plugin directories `<state>/pohunek/workflow-plugin-state/<id>/` and
`<config>/pohunek/workflow-plugin-config/<id>/`, and its commands never
contact a daemon; only the reaction dispatcher (below) reads the local daemon's
`subscribe` stream.
Every command that changes the store holds the store lock
`<state>/pohunek/workflow-plugins/store.lock` for its whole run.

```bash
pohunek workflow install work.tar.zst --sha256 <digest> --yes --json
pohunek workflow list --json
pohunek workflow inspect acme.work --json
pohunek workflow status acme.work --json
pohunek workflow enable acme.work --json
pohunek workflow disable acme.work --json
pohunek workflow uninstall acme.work --purge --json
pohunek workflow uninstall acme.work --remove-modified --json
pohunek workflow run acme.work overview --since 1d
pohunek workflow run --json acme.work overview --since 1d
```

`workflow run [--json] <plugin> <action> [<args>...]` passes every argument
after `<action>` to the action verbatim, including `--json`, `--`, `-h`,
`--host` and empty strings; pohunek's own options go before `<plugin>`.

Event reactions are a CLI process too, not a protocol surface. A plugin's
`[reactions.<name>]` table names up to 32 distinct public event names from the
Events table above, except `attach_opened` and `attach_closed`. The hidden
`pohunek workflow __dispatch` is the entry point of the dispatcher unit. It
subscribes to the local daemon's `subscribe` stream with no origin pair, and
never starts the daemon. A reaction's context is the action context, with
`invocation.kind = "reaction"` and these fields:

- `event`: `{"event": "<name>", "payload": <the event payload>}`, present
  when the whole context fits `limits.max_context_bytes`.
- `event_omitted`: `"too_large"` in place of `event` when it does not fit.
  `session_id` is then present when the payload names a session: `session_id`
  or, for session events, `session.id`.

Run records of a reaction carry `kind = "reaction"`. A `dropped` record has
`outcome = "dropped"`, no `name`, and `dropped`, the number of events one
overflow episode of a plugin's queue refused. No new event kind, daemon method,
protocol code or SDK type is added for reactions.

Every `--json` result is one standard CLI process envelope. The `ok` payloads:

- `install`: `{status, plugin, warnings?}`, where `status` is `installed`,
  `already_installed` or `root_restored`. `warnings` is present when the
  plugin was recorded but the store could not confirm the change is durable.
- `list`: `{plugins: [plugin...]}`.
- `status`: `{plugins: [{id, version, digest, enabled, units: [unit...],
  units_error?, run_log_error?}]}`, where a unit is `{kind, name, state,
  state_reason?, last_run, consecutive_failures, backoff_ms?, backoff_error?}`.
  `kind` is `job` or `service`; `state` is `running`, `stopped`, `failed` or
  `unknown`; `last_run` is `{ts, outcome, exit_code?, signal?, error_code?}` or
  `null`; `consecutive_failures` is `null` when it cannot be determined;
  `backoff_ms` and `backoff_error` are present for services only. `units_error`
  and `run_log_error` are present when the unit record or the run log cannot be
  read. The guide's [Status](knowledge/guides/workflow-plugins.md#status)
  section defines each field.
- `enable` and `disable`: the updated `plugin`, plus `warnings` from
  `disable` when it could not list the units of a plugin whose files fail
  verification and no unit record names any unit, or when no `pohunek service`
  installation is recorded, no unit is recorded, and the service manager could
  not be reached to look for leftover units. Each warning says so; a unit left
  behind refuses every run.
- `inspect`: `{plugin, description, files_dir, state_dir, config_dir,
  actions: [{name, argv, args, timeout_ms?, max_output_bytes?}],
  jobs?: [{name, argv, every_s, timeout_ms?}], services?: [{name, argv}],
  reactions?: [{name, events, argv, timeout_ms?}]}`. `argv[0]` is resolved to
  an absolute path; `args` is `none` or `passthrough`; `jobs`, `services` and
  `reactions` are present when declared.
- `run`: the action's stdout, which must be exactly one JSON document (JSON
  whitespace around it is allowed and dropped), placed as `ok` byte for byte:
  key order, number spelling and inner whitespace are the action's own. It is
  printed only when the action exited 0.
- `uninstall`: `{plugin, purged, purge?, warnings?, state_dir, config_dir}`.
  `plugin` is `null` when `--purge` removed only the directories of an id that
  has no record. `purge` is `{state_dir, config_dir}`, each `removed` or
  `absent`. `warnings` is present when the record was removed but the store
  could not delete every installed file (the warning names where the residue
  is) or confirm durability, when a leftover of an earlier purge still
  cannot be removed (the warning names its location), or for the same unit
  warnings as `disable`. No warning changes the result.

A `plugin` is `{id, version, digest, enabled, installed_at_unix_seconds}`.

Typed errors use the standard `{class, code, msg, recover?}` shape and never
contain an argv value, an environment value, manifest text or a byte of an
action's output. `workflow_action_failed` adds `exit_code` (the action's exit
code, or 128 plus the signal number) and, when a signal ended the action,
`signal`. `workflow_installed_disabled` adds `cause`, the code of the failure
that left the plugin disabled, and its `recover` is that failure's recovery
followed by the `pohunek workflow enable` step. These fields belong to the
CLI envelope, not to the protocol's error type:

| Code | Class | Meaning |
|---|---|---|
| `workflow_local_only` | `configuration` | A remote `--host` was given; nothing was read or written. |
| `workflow_archive_invalid` | `configuration` | The archive file is unreadable, too large, or not a canonical package archive. |
| `workflow_digest_mismatch` | `configuration` | The archive does not have the `--sha256` digest; nothing was installed. |
| `workflow_descriptor_invalid` | `configuration` | `workflow.toml` is missing, too large, or breaks a manifest rule, or the archive also carries `runtime.toml`. |
| `consent_required` | `configuration` | `install` ran without `--yes`; nothing was created. |
| `workflow_plugin_unknown` | `configuration` | No installed plugin has the id. |
| `workflow_plugin_installed` | `configuration` | Another archive of the id is installed. |
| `workflow_root_invalid` | `runtime` | The installed files are missing, changed or unreadable; they are left as found. The hint names the recovery for the cause: reinstall or plain `uninstall` for missing files, `uninstall --remove-modified` for changed ones. |
| `workflow_root_intact` | `runtime` | `uninstall --remove-modified` was given for installed files that verify or are missing; nothing was removed. |
| `workflow_install_uncertain` | `runtime` | An install's record replacement committed without confirmed durability and could not be read back; the plugin's directories were kept. Check `pohunek workflow list`; installing the same archive again is safe. |
| `workflow_store_busy` | `runtime` | Another workflow command holds the store lock; nothing was changed. |
| `workflow_purge_incomplete` | `runtime` | A purge did not finish; the message says whether the plugin was uninstalled, the outcome for each directory, and where remaining data is. |
| `workflow_dirs_overlap` | `configuration` | The store, state and config locations contain each other; nothing was read or written. |
| `workflow_store_failed` | `runtime` | The store or a plugin directory cannot be read or changed safely. |
| `workflow_config_invalid` | `configuration` | `<config>/pohunek/workflow.toml` is unsafe (not an owner-only `0600` regular file in a directory others cannot write), unreadable, not of the knobs schema, or holds an out-of-range value; nothing was run. |
| `workflow_plugin_disabled` | `configuration` | The plugin is disabled; nothing was run. |
| `workflow_action_unknown` | `configuration` | The manifest declares no action of the name; nothing was run. |
| `workflow_action_args_refused` | `configuration` | Arguments after the action name were given to an action declaring `args = "none"`; the message counts them and names no value; nothing was run. |
| `workflow_origin_incomplete` | `configuration` | Exactly one of `POHUNEK_SESSION_ID` and `POHUNEK_DAEMON_ID` is set; nothing was run. |
| `workflow_timeout_refused` | `configuration` | The action declares a `timeout_ms` above `limits.action_timeout_max_ms`; nothing was run. |
| `workflow_context_too_large` | `configuration` | The context JSON is larger than `limits.max_context_bytes`; nothing was run. |
| `workflow_run_log_failed` | `runtime` | The plugin's run log cannot be opened; nothing was run. |
| `workflow_spawn_failed` | `runtime` | The action's program is missing or not executable, or its process could not be started. |
| `workflow_run_failed` | `runtime` | The CLI could not observe the action or read its output; its process group was killed. |
| `workflow_action_timeout` | `runtime` | The action did not finish before its deadline and its group was stopped. |
| `workflow_output_limit` | `runtime` | Stdout or stderr passed its cap and the group was stopped; bytes past the cap were not shown. |
| `workflow_action_interrupted` | `runtime` | The CLI received `SIGINT`, `SIGTERM` or `SIGHUP` and stopped the action's group. |
| `workflow_action_failed` | `runtime` | `run --json` only: the action exited non-zero or was ended by a signal the CLI did not send; carries `exit_code` and `signal?`; its stdout was not printed. |
| `workflow_output_incomplete` | `runtime` | `run --json` only: the action exited 0, but a process that left its group still held stdout open, so the capture was never judged. |
| `workflow_output_invalid` | `runtime` | `run --json` only: the action exited 0, but its stdout is empty, not JSON (including arrays and objects nested deeper than 126 levels, so the envelope stays within the 127 levels serde_json's default parser accepts), ends inside its document, or has data after it. |
| `workflow_service_not_installed` | `configuration` | The plugin declares jobs, services or reactions, and no verified `pohunek service` installation (its `service.toml` and `<prefix>/bin/pohunek`) describes this process, or the installation records another pohunek version than this CLI, whose units would run a trampoline this CLI cannot vouch for; the message names both versions and the recovery (`pohunek service install` from this version, or an upgrade to it). `enable` and an enabling `install` change nothing; a `disable` or `uninstall` has already marked the plugin disabled when its recorded installation turns out to be unusable, and the plugin stays installed disabled. `disable` and `uninstall` accept an installation of any version. |
| `workflow_unit_refused` | `configuration` | A job's `every_s` is below `jobs.min_interval_s` or its `timeout_ms` above `jobs.timeout_max_ms`; nothing was changed or run. |
| `workflow_units_failed` | `runtime` | The service manager did not install or remove a unit. After a failed enable every unit was removed and the plugin disabled again; after a failed disable the plugin stays disabled; a failed uninstall keeps the plugin installed, disabled. Without a recorded `pohunek service` installation, a disable or uninstall that cannot reach the service manager while units are recorded fails this way and keeps the unit record. An uninstall whose unit record still names units keeps the plugin record too. |
| `workflow_installed_disabled` | `runtime` | `install --yes` only: the new plugin was recorded, but a step after that failed: reaching the service manager (`cause` `workflow_units_failed`), marking the plugin enabled (`workflow_store_busy` while another workflow command holds the store lock, `workflow_root_invalid`, or `workflow_store_failed`; for a failure that may have written the flag, the plugin is disabled again first), or installing a unit (`workflow_units_failed`). `cause` is that failure's code. When every step was undone the plugin stays installed disabled with no units and the message says so: fix the cause, then run `pohunek workflow enable <id>`. When undoing the failed step failed too (a unit install, or clearing a flag that may have been written), the message says the plugin may remain enabled or keep units: run `pohunek workflow disable <id>` until it succeeds, then `pohunek workflow enable <id>`. Every check that can refuse an enabling install, and the unit lifecycle lock, come before the record is written. |
| `workflow_unit_stale` | `runtime` | A unit names another archive than the installed one; it was refused and recorded. |
| `workflow_unit_unknown` | `configuration` | The installed manifest declares no job or service of the unit's name; nothing was run. |
| `workflow_units_installed` | `configuration` | Only `pohunek service uninstall`: a workflow plugin still has jobs or services recorded, or the reaction dispatcher unit is recorded. The message names each plugin to disable and its `pohunek workflow disable <id>`: the plugins with recorded units, and for the dispatcher each enabled plugin that declares a reaction or fails verification, or every installed plugin when none does; nothing was changed. |

`workflow run` streams the action's stdout and stderr, each up to its cap.
With `--json` it captures stdout up to the same cap instead, still streams
stderr, and prints exactly one envelope on stdout. In both modes it exits
with:

| Exit status | When |
|---|---|
| the action's status | The action exited by itself; 128 plus the signal number when a signal it was not sent by the CLI ended it. With `--json` a non-zero status is reported as `workflow_action_failed`. |
| 124 | `workflow_action_timeout`. |
| 125 | `workflow_output_limit`. |
| 128 plus the signal number | `workflow_action_interrupted`. |
| 1 | Any other workflow error, including every refusal, `workflow_output_incomplete` and `workflow_output_invalid`. |

When several apply, an output stream over its cap wins over a received
signal, which wins over the deadline; with `--json`, after a zero exit, a cut
stdout wins over stdout that is not one JSON document. The action's environment, its
`POHUNEK_PLUGIN_CONTEXT` JSON (schema 1), the content-free run log
`<state>/pohunek/workflow-plugin-state/<id>/logs/runs.jsonl` and the owner
knobs `<config>/pohunek/workflow.toml` are specified in
`docs/knowledge/guides/workflow-plugins.md`, which also holds the manifest and
trust rules, and the plugin units of jobs and services: each runs the hidden
`pohunek workflow __exec <id> <digest> job|service <name>`, an entry point
of the service manager units, not a command to call. Event reactions run
through the dispatcher unit's hidden `pohunek workflow __dispatch` (see
"Events" above and the guide's "Event reactions").

## Hermes Operator Plugin CLI

The Hermes operator plugin is a local CLI lifecycle, not a daemon public method.
Historically, M3 did not bump the then-current public protocol v2 or add a
Hermes-specific wire shape. Current builds use public protocol v4. The plugin
assets and generated skill come only from the installed, enabled and selected
official `pohunek.runtime.hermes` package; the CLI verifies them against the
package's closed operator sidecar, adds its own core hook reporter, and installs
them only into an explicitly selected Hermes profile or custom absolute home.
Without such a package, `install` and `update` fail with
`configuration/hermes_package_not_installed` and change nothing; `status`,
`doctor`, and `uninstall` keep working on an existing install.

```bash
pohunek integration install --agent hermes --hermes-profile default \
  --access-mode manage --allow-host local \
  --tool-timeout-ms 8000 --max-output-bytes 262144 \
  --max-screen-bytes 65536 --max-concurrency 1 --json
pohunek integration doctor --agent hermes --hermes-profile default --json
```

Hermes status reports `installed`, `enabled`, `modified` (a managed file differs
from the checksums its ownership marker records), `outdated` (the marker's asset
checksums, operator sidecar digest, package id, or package digest differ from
the selected package, so an untouched install from another package version or
from an older CLI's compiled copy is flagged; never set while no usable package
is selected, because no update could apply), `package_source_error` (the code
an install or update would fail with on the current package selection) and the
stale-sibling flags. The doctor's `asset_current`
check fails for an outdated plugin; `integration update` replaces it without
`--confirm-modified` unless `modified` is true.

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
Remote calls use the existing direct overlay transport. The policy is a
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
  local plus reachable daemon hosts from the standalone local overlay discovery
  cache. It does not need a local daemon to expand remote targets.
- `pohunek notifications watch`: stream `notification_created`,
  `notification_updated`, and `notification_deleted`; `--all-hosts` opens one
  subscription per reachable host. `--session`, `--kind`, and `--status` (or
  `--unread`) filter client-side on the record each create or update event
  carries; while any filter is set, `notification_deleted` events are dropped
  because they carry only the notification id.
- `pohunek notifications read|ack|archive|delete <target>`: update one record.
  Targets accept bare `id` or `host/id`; an explicit target host overrides
  `--host`.
- `pohunek notifications policy get|set`: read policy or toggle one
  provider/kind flag. `set` accepts `--provider default|<runtime-id>`, where
  `default` selects the base `enabled` flags and any valid runtime id, installed
  or not, selects that provider's override (seeded from `enabled`). `default`
  is a keyword, so it shadows a runtime literally named `default`, whose
  override can be set only through `notification.policy.set`. `set` also takes
  `--kind <kind>` and exactly one of `--enabled` or `--disabled`.
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
- The raw stream carries no acknowledgement of the input a client writes. A
  client that needs to know the program read its input observes the terminal
  output or the session activity instead.
- The end of a client's input is a shutdown of its write half (a FIN) on a
  transport that has one: a Unix or TCP socket. With the Rust SDK, call
  `AsyncWriteExt::shutdown` on the stream inside a `RawStream::Local` or
  `RawStream::Remote`. The daemon then stops reading the client, keeps
  forwarding output and closes the stream once the program's terminal accepted
  the last input frame it sent ("written to the program's terminal", not "read
  by the program"), or earlier when the program exits, the session stops, the
  lease is released, a detach by stream id arrives, the worker reports an
  error or an output write fails. Output still queued behind the
  last acknowledgement when the stream ends may not be shown. A client with no
  input closes at once. A client must keep reading until the daemon closes the
  stream.
- A complete close of the connection is a disconnect. On a Unix socket the
  daemon sees it while a paste waits for the program and during that drain, and
  ends the attach at once; bytes the terminal has not accepted are dropped. The
  TS SDK and WebSocket clients have no end of input: `writable.close()` sends
  no FIN, `RawStream.close()` and a WebSocket close are a complete close of the
  connection (a relay's close reaches a Unix-socket daemon as a complete close
  of its upstream connection). A client that must know its input arrived
  watches the output or the session activity and then detaches
  (`session.detach`) or closes. A `pohunek` CLI older than this release that
  pipes input into a slow program ends its input with a half-close followed by
  its exit, which counts as a disconnect on a Unix socket.
- On a TCP connection (a remote host, or a relay whose upstream is TCP) the
  daemon sees a client's close only when it reads or writes the connection. It
  cannot tell a complete close from a half-close: both end the client's input
  and are drained. A client that disconnects while the program neither reads
  input nor writes output keeps the attach until the program reads, the session
  stops or the lease is released.

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
TypeScript SDK), and pin the protocol version they were built against: the
TypeScript SDK handshake accepts any version inside `CLIENT_PROTOCOL_VERSIONS`
and translates an older daemon's payloads, and a daemon one release newer serves
the previous version. Core crates beyond
`pohunek-client` that a Rust client links are a pinned, not a stable, API with no
back-compat shims. Four methods
are public obligations whose callers are mostly those external UI clients, so
core keeps server-side contract tests for each of them: `host.discover` and
`worktree.remove` are called only by the UI clients, `host.resolve` by the
TypeScript SDK, and `subscribe` is also called by the CLI (`pohunek subscribe`,
`pohunek attach`, `pohunek notifications`).

- `host.discover`: socket-level coverage in
  `crates/daemon/tests/health_socket.rs`
  (`public_bind_serves_host_discover_with_supplied_registry`,
  `host_discover_lists_this_machine_as_one_local_record`) and the
  version-mismatch contract test in `crates/daemon/tests/remote_tcp.rs`. The
  previous release's record shape decoding a `local` record is covered by
  `crates/protocol/tests/host_record_previous.rs`.
- `host.resolve`: socket-level coverage in `crates/daemon/tests/host_resolve.rs`
  (peer and pinned-route answers, `local` names, the typed errors, and
  `method_not_found` over the overlay listener) and the protocol 3 refusal in
  `crates/daemon/tests/protocol_window.rs`; the TypeScript SDK side is covered by
  `sdk/ts/sdk/test/host-resolution.test.ts` and, against a real daemon,
  `sdk/ts/sdk/test/host-resolution.e2e.test.ts`.
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
- `default_overlay_registry`, `overlay_registry_from`, `OverlayConfig`,
  `OVERLAY_CONFIG_FILE`, `DEFAULT_REMOTE_PORT` and `REMOTE_PORT_ENV`: the
  overlay configuration loader and registry builder ("Overlay configuration"
  below). They moved here from `pohunek-netbird`, which is not a pinned crate.
  A configuration failure is `ClientError::OverlayConfiguration`
  (`overlay_configuration_invalid`, `configuration` class).
  `OverlayConfig::from_file()` tells whether the providers were listed in the
  file (`false` for the NetBird default of an absent file), and
  `diagnose_overlays(registry, budget)` returns the `OverlayDiagnostic` of every
  configured overlay, a provider that has not answered within `budget` reported
  as unavailable.
- `ClientError::HostUnreachable { host, overlay, address, port, source }`: a
  connection to a resolved peer failed. `overlay` is `None` when the caller
  dialled an address it resolved itself (`connect_trusted_tcp_addr*`,
  `connect_raw_tcp_addr*`). Its `transport/host_unreachable` mapping names the
  overlay, the address and the port in `msg` and, when the overlay is known,
  hints in `recover` that the remote host's `overlays.toml` must list that
  overlay.
- `Client`: framed request/response and subscription client.
- `ClientOptions`: `request_timeout` and `connect_timeout`, both defaulting to
  5 seconds, and `origin_source`, set with `with_origin_source`. A typed
  `session.new` uses a dedicated connection with a response budget of 60
  minutes plus 1 second of transport headroom. This covers the valid host
  bounds of up to 10 minutes each for worker connection, worker
  initialization, and, with initial input, reader grace, plus the create
  hooks: `pre-create` and `post-create` each run a host layer and a repository
  layer capped at 5 minutes. The rest is room for other creation work.
  `Client::session_resume` and `session_resume_with` (and the generic
  `call::<SessionResume>`) also use a dedicated connection, with a response
  budget of 110 minutes plus 1 second of transport headroom: eleven lifecycle
  steps at the longest valid daemon bound of 10 minutes each. They are the
  retirement of the lost generation's job (the worker exit timeout), the
  cleanup sweep (up to three signalling passes, each waiting out the sweep
  grace after `SIGTERM` and again after `SIGKILL`, plus one settle window), the
  new worker's connection and initialization, and one step for the rest of the
  resume. A resume that outlasts the ordinary 5-second timeout therefore returns its real
  result or refusal. A deadline that still elapses returns
  `ClientError::SessionResumeOutcomeUnknown` (`session_resume_outcome_unknown`,
  transport class) instead of `request_timeout`: the daemon does not cancel the
  resume when the client leaves, so inspect the session before retrying.
  `session_fork`, `session_stop`, `session_remove` and
  `session_remove_accepting_unconfirmed` (and their generic `call::<...>`
  forms) use dedicated connections too. A fork gets 30 minutes (three
  lifecycle steps: worker connection, worker initialization, the rest), a stop
  90 minutes (nine: the worker's stop or a conflicted runtime's job
  retirement, the cleanup sweep of up to three signalling passes, the rest),
  and a remove the stop's 90 minutes plus four hook runs at the daemon's
  5-minute hook timeout (the `pre-remove` and `post-remove` events, each with a
  host and a repository layer), 110 minutes. Each adds 1 second of transport
  headroom. An elapsed deadline returns `ClientError::SessionForkOutcomeUnknown`
  (`session_fork_outcome_unknown`; a retried fork creates a second child),
  `SessionStopOutcomeUnknown` (`session_stop_outcome_unknown`; a repeated stop
  is idempotent) or `SessionRemoveOutcomeUnknown`
  (`session_remove_outcome_unknown`), all transport class. A stop or remove
  queued behind a resume or fork of the same session waits for that
  operation's lifecycle lock and can exceed any budget; that is an unknown
  outcome too, and the daemon completes the queued call after the client left.
  The daemon bounds its own wait for a worker's stop answer by the stop grace
  (four times, for the stop and the marker sweep), the kill reap deadline and
  the worker's terminal-commit wait plus a margin, after waiting up to 10 minutes for an earlier exchange on the
  worker's control connection (up to 80 minutes behind an input write, which
  the worker delivers on its own stall policy); a worker silent past that ends the stop or
  removal with `worker_operation_failed`. The Hermes integration passes an explicit
  `request_timeout_ms`, which keeps overriding these budgets.
  `with_request_timeout` explicitly overrides
  these budgets, including with a shorter deadline; assigning a nondefault value
  to the public `ClientOptions.request_timeout` field has the same effect.
  Dropping the Rust request
  future ends the client's wait; it does not prove the daemon canceled a
  creation already in progress. Raw envelope requests retain the ordinary
  timeout.
  A request that times out, or is answered with another request's reply,
  leaves its connection unusable, because a late reply could be read as the
  answer to a later request. That call fails (`request_timeout`, or a framing
  error for the mismatched reply) and is never sent again by the client. The
  next `request` or `call` on the same `Client` opens a fresh connection to the
  same endpoint, learns the daemon's protocol version again (the daemon may
  have restarted on another version) and sends only its own request; the old
  connection is dropped, so its late reply is never delivered. A reconnect that
  fails returns the connect error and the following call tries again.
- `ConnectionOptions.request_timeout` in the assistant host API is `None` by
  default, preserving these SDK budgets. `Some(duration)` is an explicit
  override even when `duration` equals the ordinary 5-second default. An
  omitted serialized field also means `None`.
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
  still have completed remotely. A `session_resume`, `session_fork`,
  `session_stop` or `session_remove` whose deadline elapses is
  `SessionResumeOutcomeUnknown`, `SessionForkOutcomeUnknown`,
  `SessionStopOutcomeUnknown` or `SessionRemoveOutcomeUnknown` instead; see
  `ClientOptions` above.
- `next_request_id(method)`: shared correlation-id generator used by SDK-backed
  clients.
- `remote_host_with_port(host, port)`: formats a provider-resolved selector
  with an explicit non-zero daemon port.
- `ConfiguredTransport` and `OverlayRegistry`: validated provider registry with
  stable overlay IDs and one non-zero daemon port per provider.
- `OverlayTransport`: provider contract whose required typed identity resolver
  must match only the requested peer-ID or FQDN field; implementations cannot
  inherit an untyped selector fallback.
  `discover_snapshot` reads the visible peers and this machine's own identity in
  one pass; a provider that derives both from one state read overrides it, so
  discovery runs the provider once and the `local` record and the peers cannot
  disagree (the default reports the peers only, so a provider without the override adds no local record).
- `discover_hosts(registry)`: aggregated configured-overlay discovery with
  default bounded probes.
- `discover_hosts_with_options(registry, options)`: the same discovery with an
  explicit per-probe timeout, overall deadline, and concurrency bound. Provider
  failures are isolated unless every configured overlay fails. Providers return
  remote peers; the machine itself is added as a `local: true` record for each
  overlay that reports its identity, and clients that fan out add their
  Unix-socket target explicitly and skip that record.
- Raw and attach helpers: `connect_raw*` and `attach_raw*`.

Overlay configuration: `default_overlay_registry()` builds the registry that
the CLI, the daemon and external Rust clients share. It reads
`<config>/pohunek/overlays.toml` (`XDG_CONFIG_HOME`, else `~/.config`):

```toml
providers = ["netbird"]   # exactly one overlay provider per host: "netbird" or "tailscale"
port = 18722              # optional; TCP port of the remote control listener
```

The file is optional. When it or its directory is absent the host works over
`netbird` on port 18722. When nothing exists at the file path (no file, no symlink) the defaults apply
and the directory is not inspected: there is no configuration to trust, and a
writer in an unsafe directory can only make a file appear, which is then
refused, or delete it, which falls back to the NetBird default. When anything
exists there, every component of the directory path must be a real directory
(no symlink) owned by the user or root and not writable by other accounts, and
the file must be
a regular file owned by the user with mode `0600` and at most 64 KiB, in
UTF-8. Parsing is strict: unknown keys and wrong types fail. `providers` takes
exactly one known id (`netbird` or `tailscale`); several providers on one host are not
supported yet and fail with a message that points to
[#1113](https://github.com/zajca/pohunek/issues/1113). The environment variable
`POHUNEK_REMOTE_PORT` overrides `port` for development and tests; a service-started
daemon does not receive it, so a port that must reach both the daemon and the
CLI belongs in the file. Every failure, including an unusable
`POHUNEK_REMOTE_PORT`, is `overlay_configuration_invalid` (class
`configuration`; before this code an unusable `POHUNEK_REMOTE_PORT` reported
`netbird_configuration_invalid`). Its message names the file and at most a line
number, never a value from the file, and no provider is consulted before it is
raised. The daemon validates the file before it takes a lock, migrates the store
or adopts a worker, and exits with the same code in its error text. The daemon
reads the file at start, so a change applies after a daemon restart. A CLI of
this release reads `port` from the file while an older daemon still binds the
environment value or 18722, and so does a client of an earlier release (a CLI
on another host, or a Rust client pinned to an earlier tag such as the
`pohunek-work` gateway): set `port` only after every daemon and every client that
dials those hosts runs this release and the daemons are restarted. A pinned
client adopts the file when it bumps its pin.

Diagnostics. `pohunek doctor` checks the overlays that are configured instead
of NetBird unconditionally: one `<overlay>_cli` check per overlay, in the place
`netbird_cli` held (see `daemon.doctor`). It also reports `overlay_config`: `fail`
when `overlays.toml` is refused or `POHUNEK_REMOTE_PORT` is unusable (the message
names which, and no overlay is probed), `warn` when the running daemon serves another
overlay or port than the file configures, with the text "restart the daemon to
apply", and `ok` otherwise. A daemon that reports no overlay (an older release)
or does not answer is not compared, except that a daemon that answers with no overlay check while `overlays.toml` is present is flagged the same way: it ignores the file. When `POHUNEK_REMOTE_PORT` is set in the
shell that runs `pohunek doctor`, the warning names it, because it overrides the
file's `port` for that process only.

The daemon library ties the doctor's severity to the registry it serves:
`DaemonState::new`, `DaemonState::new_with_discovery` and `ControlServer::bind`
take an `OverlaySource` (`File` when `overlays.toml` listed the providers, `Default`
for the NetBird default of an absent file; `OverlaySource::of(&OverlayConfig)`
derives it), so no state carries a source its registry did not come from.

The `tailscale` provider reads `tailscale status --json` from the local
Tailscale CLI, found through the same trusted-executable lookup as the NetBird
CLI (a missing or untrusted binary is `tailscale_cli_missing`). The listener
address is the node's IPv4 address from `Self.TailscaleIPs`; without one, or
while `BackendState` is not `Running`, or in userspace networking (`TUN` false),
the listener has no address (`tailscale_listener_address_missing`) and the
discovery and resolution calls fail with `tailscale_state_unavailable`, because
a dial in the `100.64.0.0/10` range would otherwise leave through the default
route. A peer is selectable when it is in the network map, its key has not
expired, it is not flagged `ShareeNode` (a node of a user this tailnet shared a machine to), and the
status lists an address Tailscale assigns for that exact peer: IPv4 in
`100.64.0.0/10` or IPv6 in `fd7a:115c:a1e0::/48`, never the ChromeOS block
`100.115.92.0/23`, the service addresses `100.100.100.100` and
`fd7a:115c:a1e0::53`, or the 4via6 range. Tagged peers are selectable. The
`peer_id` is the stable node `ID` and survives re-keying; the name is the
MagicDNS name without its trailing dot and the short name is its first label.
`HostName` is never used. A peer is dialed on its IPv4 address when it has one,
else on its IPv6 address; an offline peer is listed without its address and can
still be named explicitly. Tailnet ACLs are the access control: the listener
authenticates nothing beyond reachability.

Public daemon construction is registry-explicit. `DaemonState::new` and
`ControlServer::bind` require a validated `OverlayRegistry`, and the shared
discovery cache cannot be constructed without one. This keeps `host.discover`
usable through the public server constructor instead of creating a latent
registry-less state.

Connection APIs:

- `Client::connect(host, socket_path)`: `""` and `"local"` use the Unix socket;
  any other host is resolved through the default configured overlay registry.
  This machine's own overlay name selects the Unix socket like `local`, a name
  it shares with a peer is `overlay_host_ambiguous`, and an explicit `@<port>`
  route always names a peer. A peer is dialed over its exact per-overlay route. `<overlay>:<selector>` restricts
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
- `names_this_machine(host, connect_timeout)` and `names_this_machine_in(host,
  connect_timeout, &registry)`: whether a host name is this machine's own
  overlay identity, resolved with `OverlayRegistry::resolve_target`. A
  provider's `resolve_selector` matches its own identity at the same steps as
  a peer's, so a name both carry is `overlay_host_ambiguous`, whose message
  lists every matching host and whose `recover` text names the
  selector of each: provider-qualified for a peer, `local` for this machine. `local`, an
  unknown name, an unavailable provider, and a `@<port>` route answer `false`.
  `Client::connect` applies the same rule, so the CLI's alias table only saves
  a lookup (see `docs/cli.md`, host resolution).
- `resolve_host_target(host, connect_timeout)` and `resolve_host_target_in(host,
  connect_timeout, &registry)`: resolve a host once to a `ResolvedHost`, which is
  `Local` or `Remote { pinned_route, display }`. `pinned_route` is
  `<overlay>:peer~<id>@<port>` (`fqdn~` when the provider reports no peer id), so
  connecting to it looks the peer up by identity and a renamed or reassigned
  short name cannot select another peer; the address may change. `display` is the
  name the caller typed. A peer with neither a peer id nor an FQDN fails with
  `host_identity_unpinnable`. `pinned_host_id(overlay, peer_id, fqdn)` builds the
  `<overlay>:<selector>` part that fan-out targets share.
- `Client::connect_resolved_with_options(&resolved, socket_path, options)`:
  connects to a `ResolvedHost`; lookup failures and connection errors name
  `display`. Use it for every connection of one logical operation.
- Assistant: `HostConfig::pinned(id, host, pinned_route)` carries a pinned
  remote host through `prepare_with_options` and `start_prepared_with_options`;
  its `attach_host()` is the pinned route.
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
- `Client::session_wait_turn(SessionWaitTurnParams)`: opens a dedicated
  connection whose response deadline is `timeout_ms` plus transport headroom,
  or `MAX_SESSION_WAIT_TURN_MS` plus headroom when the request leaves the
  duration to the daemon default, so the client deadline never cuts a valid
  wait short.
- `Client::host_governance_inspect()`: calls `host.governance.inspect` with
  exact null parameters and returns the safe `HostGovernanceStatus` projection.
  It has no local cache or mutation behavior.
- `Client::session_resume`, `session_resize`, and `session_set_metadata`: typed
  lifecycle helpers used by automation clients.
- `Client::integration_status(IntegrationStatusParams)`: reads the complete
  daemon-managed Codex/Claude install contract without mutation.
- `Client::request(&Request) -> serde_json::Value`: sends one request and returns
  the `ok` payload in the current shape for low-level callers and framing tests;
  against a daemon of the previous protocol version it translates both ways, and
  fails with `ClientError::DaemonProtocolTooOld` before sending a method that
  version never defined.
- `Client::subscribe(&Request) -> Subscription`: consumes the client connection
  after a subscribe ack.
- `Subscription::next_line() -> Option<String>`: returns event JSON lines in the
  current shape; a connection that selected the previous protocol version
  translates each event first.
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
  control channels and `raw()` for unframed attach channels, and the optional
  `relayed` member. `relayed` must be `false` on a transport whose frames do not
  pass through a relay service (`SocketTransport` sets it); `WsTransport` sets it
  to `true`. A transport that omits it is treated as a relay, so a custom or
  wrapping transport must pass the wrapped transport's value through: otherwise
  every `session.new` with `secret_env` fails with
  `session_new_secret_env_relay_refused`.
- `ControlChannel`: `send(line)`, async `lines`, and `close()` for one framed
  control connection.
- `RawDuplex`: `ReadableStream<Uint8Array>`, `WritableStream<Uint8Array>`, and
  `close()` for one raw attach connection.
- `ConnectOptions`: TypeScript counterpart to Rust `ClientOptions`; the package
  does not export a separate `ClientOptions` alias. It carries
  `connectTimeoutMs` and `requestTimeoutMs`, both defaulting to 5000 ms, plus an
  optional validated `origin: {sessionId, daemonId}` pair. A typed `session.new`
  uses the same dedicated 60-minute response budget and 1-second transport
  headroom unless `requestTimeoutMs` is explicitly supplied;
  an explicit value replaces the dedicated budget. A timed-out creation may
  still have completed on the daemon, so inspect sessions before any retry.
  Typed `session.resume`, `session.fork`, `session.stop`, `session.remove` and
  `session.remove_accepting_unconfirmed` calls also use dedicated connections
  (budgets exported as `SESSION_RESUME_REQUEST_BUDGET_MS`,
  `SESSION_FORK_REQUEST_BUDGET_MS`, `SESSION_STOP_REQUEST_BUDGET_MS` and
  `SESSION_REMOVE_REQUEST_BUDGET_MS`, equal to the Rust client's) and report an
  elapsed deadline as the `ClientError` kinds `sessionResumeOutcomeUnknown`,
  `sessionForkOutcomeUnknown`, `sessionStopOutcomeUnknown` and
  `sessionRemoveOutcomeUnknown` (codes as in the Rust client). A consumer that
  switches exhaustively on `ClientError.kind` must handle the new kinds.
  The typed call opts into the configured host reader grace when it sends
  initial input.
  A request that times out, is answered with another request's id, or is
  answered with an unparsable line (or whose `subscribe` fails) leaves its
  control connection unusable, because a late reply could be read as the answer
  to a later request. That call fails with `requestTimeout`, `framing` or
  `json` and is never sent again by the client, reads included: the caller
  decides whether to retry. The next call on the same `Client` opens a fresh
  control connection through the transport, learns the daemon's protocol
  version again and sends only its own request; the old channel is closed
  (without waiting longer than `connectTimeoutMs` for it), so its late reply is
  never delivered, and concurrent callers share one reconnect. A reconnect that
  fails rejects with the connect error and the following call tries again. A
  closed client, a client whose subscription took the channel and a connection
  that selected an incompatible protocol version stay refused. A version
  probe on a fresh connection has its own `requestTimeoutMs`, so a call that
  probes can take up to twice that; an unanswered probe rejects with
  `requestTimeout` naming the probe. Dedicated calls (`session.new`, `wait`,
  `session.stop`, ...) are unaffected: they open their own connection and do
  not depend on the shared one.
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
- `CompatError`, `downgradeRequestParams`, `upgradeResult`, and
  `upgradeEventPayload`: the shape-only adapter between the current protocol and
  the previous version that `Client` applies on a connection that negotiated the
  previous version; also exported for low-level callers.
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
- Host resolution (root entry only): `resolveHost`, `connectHost`, and the
  `ResolvedHost` type.
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
  overlay TCP directly.

Connection APIs:

- `Client.defaultOptions()`: returns resolved default timeouts.
- `Client.connectWs(baseUrl, host, opts?)`: WebSocket relay transport.
  `baseUrl` may use
  `http`, `https`, `ws`, or `wss`; the SDK connects to
  `/daemon/<host>/control` under that base URL.
- `Client.connectTransport(transport, opts?, remoteHost?)`: injection point for
  tests and custom transports that implement `Transport`. The transport's
  `relayed` member decides whether `secret_env` may be sent (see `Transport`).
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
- `client.sessionWaitTurn(params)` and generic typed
  `client.call("session.wait_turn", params)`: open a dedicated connection whose
  response deadline is `timeout_ms` plus fixed headroom, or
  `MAX_SESSION_WAIT_TURN_MS` plus headroom when `timeout_ms` is omitted, so
  `requestTimeoutMs` never cuts a valid turn wait short. A `timeout_ms` that is
  not a nonzero `u32` fails with `daemon/bad_request` before sending, because
  JSON would serialize `NaN` or `Infinity` as `null`, which the daemon reads as
  "use the default".
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
- Root-entry `attachRaw(host, socketPath, streamId, opts?)` redeems the stream
  on the daemon `host` names. `""` and `"local"` use `socketPath`; any other
  route is resolved once through the local daemon (see "Host resolution"), and a
  route that names this machine's own overlay name redeems on `socketPath`.
- Root-entry `attachRawLocal` and `attachRawTcp`, plus shared `attachRawWs`, open
  a raw channel, write exactly one attach prelude, parse a failed redemption
  response as `ClientError`, and otherwise return the raw attach stream.

Host resolution:

- `resolveHost(route, socketPath, opts?)` maps a host route as `--host` accepts
  it to a `ResolvedHost`: `{target: "local"}`, or `{target: "remote", display,
  address: {host, port}, pinnedRoute, overlay, peerId?, fqdn?}`. `""` and
  `"local"` answer without contacting the daemon. Any other route is resolved by
  the local daemon on `socketPath` through `host.resolve`, so the selector rules
  (provider-qualified selectors, `peer~`/`fqdn~` identities, `@port`, ambiguity,
  this machine's own overlay name) are the CLI's and the Rust client's; the SDK
  holds no copy of them. The daemon's typed errors keep their `code`, message
  and recovery hint (`host_unknown`, `overlay_host_ambiguous`,
  `overlay_host_unavailable`, `host_identity_unpinnable`), and a daemon of the
  previous protocol version fails with `daemon_protocol_too_old`. `display` is
  the route as the caller typed it.
- `pinnedRoute` names the peer by its stable identity. Resolving it again
  selects the same peer after a name or address change, or fails with the typed
  unknown-host error if the peer is gone; it never selects another peer. A
  caller that opens several connections for one operation resolves once and
  reuses the endpoint.
- `connectHost(route, socketPath, opts?)` resolves the route once and connects:
  to `socketPath` for a local route, otherwise to the resolved peer with
  `connectTcp`. `host.resolve` is served on the local Unix socket only, so
  `socketPath` must name the local daemon.
- The browser entry does not export these functions; it has no Unix socket.

SDK error mapping:

- Daemon protocol errors are preserved as `ClientError.kind === "protocol"` or
  `"remoteProtocol"` and retain the daemon's original `class`, `code`, `msg`,
  and `recover` fields, except for the local TypeScript decoder's
  `daemon/host_governance_inspect_contract_mismatch` result. That redacted
  contract-mismatch error is SDK-originated even though it uses the normal
  structured protocol shape, as are the pre-send parameter refusals of the
  waiting helpers (`runtime/session_input_invalid_wait` and
  `runtime/session_wait_limit_exceeded` from `sessionInput`,
  `daemon/bad_request` from `sessionWaitTurn`).
- Most SDK-originated errors map into the public protocol taxonomy through
  `ClientErrorClass` and `ClientErrorCode`: `daemon_unreachable`, `framing`,
  `host_unreachable`, `remote_daemon_unavailable`, `request_timeout`, `io_error`,
  `json_error`, `session_input_wait_contract_mismatch`, `version_mismatch`,
  `daemon_protocol_too_old`, `version_translation_failed`,
  `session_new_secret_env_unsupported`,
  `session_new_secret_env_relay_refused`, `command_unsupported` and
  `package_install_commands_unsupported`. `daemon_protocol_too_old` and
  `version_translation_failed` are the window errors: `ClientError.kind` `daemonProtocolTooOld` carries
  `tooOld.{host, method, daemonVersion, requiredVersion}` and is raised before a
  method new in the current protocol is sent to a daemon that negotiated the
  previous one.
  `host_governance_inspect_contract_mismatch` is the separate fixed, redacted
  SDK-originated governance-response validation error.
- `session.new` with a non-empty `secret_env` (through `Client.call` or
  `Client.request` or `Client.subscribe`) is gated by the SDK before the request leaves the process:
  - On a relay (`Client.connectWs`, or any transport whose `relayed` is not
    `false`) the call fails with
    `configuration/session_new_secret_env_relay_refused` (`ClientError.kind`
    `sessionNewSecretEnvRelayRefused`) before a connection opens, because the
    relay must never see raw secrets.
  - Otherwise the SDK sends `host.inspect {skip_runtime_probes: true}` on the
    connection that then carries `session.new`. Unless the daemon reports
    `session_new_secret_env_supported`, the call fails with
    `daemon/session_new_secret_env_unsupported` (`ClientError.kind`
    `sessionNewSecretEnvUnsupported`) and no `session.new` is sent, because an
    older daemon would drop the field silently. The result is remembered per
    connection, so the check and the send always use the same daemon connection.
    A version-3 request carrying `secret_env` is also refused by the request
    adapter (`daemon_protocol_too_old`).
  - Both errors use fixed text that names no variable and no value, and no
    `ClientError` field, message or stack carries the `secret_env` contents. An
    empty `secret_env` is treated as absent and skips the check.
- `Client.runCommand({ package, command, timeout_ms?, max_output_bytes? })`
  (also `Client.call("command.run", ...)`) runs one package-declared command
  and returns `CommandRunResult`. Only those four fields reach the wire; the
  SDK never forwards arguments, environment, working directory or stdin.
  - The call uses a dedicated connection whose deadline is the wire timeout
    (`MAX_COMMAND_TIMEOUT_MS` when omitted) plus response headroom, so the
    budget of other calls is unchanged.
  - A `timeout_ms` or `max_output_bytes` that is not a nonzero `u32` fails with
    `daemon/bad_request` before anything is sent.
  - A `command_*` daemon error keeps its `class`, `code` and `msg` (a remote
    host prefixes the `msg` with `host '<route>':` and the kind is
    `remoteProtocol`).
    `ClientError.details` returns a copy of the structured detail the daemon
    attached, for example `{ process_exit: { exit_code } }` or
    `{ process_exit: { signal } }` for `command_failed`; it is `undefined`
    otherwise, and `toProtocolError()` carries it too.
  - A daemon that answers `method_not_found`, or whose negotiated protocol
    version never defined `command.run`, fails with
    `daemon/command_unsupported` (`ClientError.kind` `commandUnsupported`).
- `package.install` and `package.link` with `accept_commands` set are gated by
  the SDK like `secret_env`: the SDK sends `host.inspect
  {skip_runtime_probes: true}` on the connection that carries the request and
  sends `accept_commands` only when the daemon reports
  `package_install_commands_supported`. Otherwise the call fails with
  `daemon/package_install_commands_unsupported` (`ClientError.kind`
  `packageInstallCommandsUnsupported`) and nothing is sent, because an older
  daemon rejects the key. `accept_commands: false` is the default and is left
  off the wire without a capability check.
- `ClientError.toProtocolError()` returns the structured error
  (`ObservedProtocolError`, a `ProtocolError` whose `class` may be `"unknown"`)
  for CLI/API rendering, and `recoverHint()` returns the optional recovery text.
  `errorClass` is `"unknown"` for an error whose class a newer daemon defines
  and this SDK does not; its `code`, `msg` and recovery text are the daemon's.

The Bun-only `@pohunek/testkit/bun-relay` subpath (not the root entry, which
also runs on Node) exports the loopback-only test relay used by the SDK
transport tests:
`startTestRelay({bindHost, port, targets})` returns a `TestRelayHandle`
(`url`, `port`, `close()`), with the `DaemonTarget`, `DaemonTargetSource`, and
`StartTestRelayOptions` types. `bindHost` must be a loopback address; any other
bind fails. The helper implements the WebSocket framing documented above as a
transparent 1:1 tunnel and is not a deployable relay or `pohunek-relayd`.
