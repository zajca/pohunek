# RFC: Optional Team Relay Control Plane

- Status: Accepted for implementation
- Date: 2026-09-02
- Tracking issue: [#80](https://github.com/zajca/pohunek/issues/80)
- Umbrella issue: [#56](https://github.com/zajca/pohunek/issues/56)
- Related issues: [#69](https://github.com/zajca/pohunek/issues/69), [#70](https://github.com/zajca/pohunek/issues/70), [#71](https://github.com/zajca/pohunek/issues/71), [#72](https://github.com/zajca/pohunek/issues/72), [#73](https://github.com/zajca/pohunek/issues/73), [#81](https://github.com/zajca/pohunek/issues/81), [#82](https://github.com/zajca/pohunek/issues/82), [#83](https://github.com/zajca/pohunek/issues/83), [#84](https://github.com/zajca/pohunek/issues/84), [#85](https://github.com/zajca/pohunek/issues/85), [#86](https://github.com/zajca/pohunek/issues/86), [#87](https://github.com/zajca/pohunek/issues/87), and [#88](https://github.com/zajca/pohunek/issues/88)

## 1. Summary

Pohunek gains an optional public team relay without replacing either of its
existing operating modes. A host can continue to run `pohunekd` by itself, a
set of hosts can continue to use direct NetBird connections, and any one or more
of those hosts can additionally enroll with one relay.

The relay is a new Rust binary, `pohunek-relayd`. It owns public user and service
account authentication, teams, roles, session sharing, routing, aggregation,
audit, quotas, and the browser-facing API. It does not own PTYs or host session
state. Each `pohunekd` remains authoritative for the sessions and processes on
its machine.

The relay publishes a WireGuard endpoint. `pohunekd` embeds a userspace
WireGuard implementation and initiates both the tunnel and every application
connection to the relay. The relay never dials a host. This requires neither a
public host port nor a kernel interface nor `CAP_NET_ADMIN`.

The daemon does not authenticate or authorize end users. It authenticates the
enrolled relay and enforces host-local `HostShare` limits. A trusted relay may
multiplex users and service accounts over that host connection. End-user
identity may be retained as opaque session attribution, but it is not an input
to daemon authorization.

This RFC replaces the team-related assumptions in the current architecture.
The direct local and NetBird design remains valid and supported.

## 2. Decision record

The following decisions are final for the first complete team-relay release:

1. The relay is additive and optional. Standalone and direct NetBird modes stay
   supported and do not depend on it.
2. The relay is a separate Rust binary named `pohunek-relayd` in the existing
   Cargo workspace.
3. `pohunekd` remains one binary. There is no connector sidecar.
4. The relay exposes a public WireGuard UDP endpoint. Hosts initiate all tunnel,
   control, subscription, and attach connections.
5. WireGuard and IPv4/TCP run entirely in process. No TUN device, kernel route,
   privileged helper, or network capability is required.
6. The initial implementation uses exact released `boringtun` and `smoltcp`
   versions pinned in `Cargo.lock`. Issue
   [#72](https://github.com/zajca/pohunek/issues/72) selects them only after the
   recorded compatibility and security review defined below; every upgrade
   repeats that review.
7. The host link reuses Pohunek's typed NDJSON request/response/event model and
   separate raw attach streams. It does not expose a second daemon API.
8. A host has at most one active relay enrollment. One enrolled host can expose
   multiple independent shares to multiple teams on that relay.
9. Human and host enrollment uses OIDC device authorization when available and
   an exact loopback browser callback otherwise. WireGuard private keys are
   generated and retained on the host.
10. `pohunekd` knows the relay and `HostShare`, but not end-user identities,
    groups, roles, or session ACLs.
11. Local and direct-NetBird sessions are never published to or controllable by
    the relay. Only sessions created through an active `HostShare` are eligible.
12. A new relay-created session is minimally discoverable to its team. Reading
    metadata or terminal state, attaching, sending input, lifecycle control, or
    sharing requires an explicit permission.
13. Host ownership is exactly one principal or one team. Ownership transfer
    requires only explicit local confirmation on the host.
14. Relay persistence is PostgreSQL-only. The relay never persists PTY output,
    input, prompts, file contents, or terminal snapshots.
15. State synchronization uses subscription-first plus an atomic snapshot and
    watermark. There is no daemon replay log for the relay. Any gap forces a
    bounded full resynchronization for that host.
16. A relay outage does not stop sessions. Revoking a share removes relay access
    immediately but does not kill its sessions; they become owner-only.
17. The relay process and operator are trusted with transient plaintext and the
    full authority of every active `HostShare`. The design does not claim
    protection from a compromised relay.

## 3. Goals

- Let authenticated teams use explicitly contributed host capacity without
  making those hosts publicly reachable.
- Preserve the current local and direct NetBird workflows without a relay
  dependency or compatibility shim.
- Keep the PTY, process, worktree, and durable logical session authority on the
  host that runs them.
- Give a host owner a local, default-deny boundary for every team share.
- Centralize user, service-account, team, role, sharing, audit, and quota logic
  in one relay authority.
- Support one host in multiple teams without enrolling it in multiple relays.
- Recover relay state deterministically after restarts, disconnects, lag, and
  daemon replacement.
- Bound every public input, queue, cache, stream, retry loop, and database use.
- Keep sensitive terminal content out of durable relay storage and structured
  telemetry.

## 4. Non-goals

- Replacing direct local or NetBird access.
- Publishing local or direct-NetBird sessions through a relay.
- Protecting a host from a compromised relay within active `HostShare` limits.
- Protecting the daemon owner's Unix account from a malicious repository,
  agent, command, or collaborator running in a direct-host profile.
- End-to-end encryption that hides PTY plaintext from the relay.
- Persisted terminal recording, scrollback, prompts, input, or file contents on
  the relay.
- Allowing remote clients to submit programs, argv, environment variables,
  arbitrary paths, profile bodies, runtime definitions, mounts, or secrets.
- High availability or horizontal WireGuard termination in the first relay
  deployment. One relay identity has one active `pohunek-relayd` process.
- Provider token storage or webhook delivery. Those remain post-release work in
  [#73](https://github.com/zajca/pohunek/issues/73).
- Workload isolation. Real container or VM runtime profiles remain tracked by
  [#88](https://github.com/zajca/pohunek/issues/88).

## 5. Terminology

| Term | Meaning |
|---|---|
| `HostId` | Stable opaque identity generated once by a host and independent of hostname, address, daemon process, or relay. |
| `RelayId` | Stable identity of one relay deployment. |
| principal | A human OIDC identity or a service account. |
| team | A tenant boundary containing principals, groups, roles, hosts, shares, and relay-created sessions. |
| host owner | Exactly one principal or team with governance authority over a registered host. |
| local host operator | The same-UID operator authorized by the existing Unix socket and owner-only files; this actor confirms host-side governance changes but is not a relay principal known to `pohunekd`. |
| `HostShare` | Host-approved, team-bound capability ceiling for relay operations on one host. |
| owner path | The existing local Unix socket or direct overlay/NetBird path. |
| relay path | A host-initiated control or attach stream inside the enrolled userspace WireGuard tunnel. |
| relay-created session | A session whose immutable origin names the relay enrollment and `HostShare`. |
| session ACL | Relay-owned grants controlling access by principals, groups, and roles to a relay-created session. |

## 6. Supported topologies

All four topologies are first-class and may coexist:

```text
1. Standalone

client -- Unix socket --> pohunekd --> pohunek-sessiond

2. Direct owner mesh

client -- NetBird TCP --> pohunekd --> pohunek-sessiond

3. One standalone host plus relay

client -- HTTPS/WSS --> pohunek-relayd
                           ^
                           | host-initiated userspace WireGuard + TCP
                           |
                       pohunekd --> pohunek-sessiond

4. NetBird mesh plus optional relay enrollment

owner client -- NetBird TCP -----------------------+
                                                    v
team client -- HTTPS/WSS --> pohunek-relayd <---- pohunekd
                                                    |
other owner clients -- NetBird TCP --> other pohunekd hosts
```

The relay discovers no host implicitly through NetBird. A host is relay-visible
only after explicit enrollment and exposes only locally approved shares.

## 7. Component boundaries

| Component | Authoritative responsibilities | Must not own |
|---|---|---|
| `pohunekd` | Host identity, relay enrollment, `HostShare` policy, session origin, PTY/session routing, profile/project validation, host resource limits | End-user login, teams, groups, roles, end-user ACL decisions |
| `pohunek-sessiond` | One live PTY, child lifecycle, output ring, terminal state, input ordering | Relay, user, team, or share policy |
| `pohunek-relayd` | OIDC and service-account auth, teams, roles, groups, host registry, share requests, user authorization, routing, state catalog, audit, quotas, public API | Host PTYs, host worktrees, host profile bodies, durable terminal content |
| `pohunek` | Explicit local/NetBird owner mode and explicit relay client mode | Hidden fallback between trust domains |
| `pohunek-gui` | Existing direct-owner client behavior; future typed relay client behavior | Authorization authority |
| `web/frontend` | Browser presentation and relay client state | Authentication secrets, authorization authority, durable terminal data |
| `web/backend` | No production authority after migration; removed as a runtime service | Relay routing, auth, aggregation, or host dialing |

`pohunek-relayd` serves the compiled SPA and the authenticated HTTP/WebSocket
API. Bun remains the build, test, and development runtime for web packages, but
there is no separately deployed Bun backend with overlapping authority.

### 7.1 Trust-boundary data flow

The following flows are normative. Arrows show who initiates an application
connection or authenticated transaction, not merely packet direction after a
connection exists.

```text
Enrollment
local host operator -> pohunekd -> HTTPS/OIDC -> pohunek-relayd -> PostgreSQL
                              \-> local HostId, key, enrollment, owner record

Steady host link and synchronization
pohunekd -> userspace WireGuard -> relay UDP endpoint
pohunekd -> TCP inside tunnel -> pohunek-relayd
          subscribe -> atomic host snapshot + watermark -> ordered events

Relay request routing
principal -> HTTPS/WSS -> relay auth + team/session ACL
          -> exactly one HostShareId + revision -> existing host link
          -> pohunekd origin + local HostShare ceiling -> session worker

Terminal attach
principal -> WSS -> relay one-use attach authorization
          -> existing host control link -> pohunekd
pohunekd -> separate TCP stream inside the same tunnel -> pohunek-relayd
pohunek-relayd <-> principal WSS         (bounded opaque PTY bytes)
```

The relay never opens a connection to a host address. PostgreSQL participates
in relay authentication, authorization, audit, and catalog transactions, but
never receives terminal bytes or host private keys.

## 8. Trust and threat model

### 8.1 Trusted entities

- The local host operator controls `pohunekd`, its files, profiles, projects,
  and local approval commands. This same-UID authority is distinct from the
  relay's registered principal-or-team owner record.
- The enrolled relay process and its operator are trusted for all authority
  explicitly granted by active `HostShare` records.
- The configured OIDC issuer is trusted to authenticate human principals and
  provide stable subject identifiers.
- Team owners and administrators are trusted to manage their team's principals,
  roles, service accounts, and session ACLs within the team boundary.

### 8.2 Untrusted entities and inputs

- Public Internet clients, unauthenticated relay callers, browser input, API
  tokens, OIDC callbacks, and WebSocket frames.
- User-supplied labels, branch names, initial input, terminal dimensions, and
  allowlisted metadata.
- Repository contents, agent output, terminal escape sequences, hook reports,
  and provider data.
- Hostnames, IP addresses, claimed `HostId` values, claimed team/share IDs, and
  reconnect state until bound to authenticated enrollment state.
- A normal relay infrastructure administrator using application APIs. This role
  has no implicit session-content permission, although the trusted process
  operator can technically access plaintext and secrets at runtime.

### 8.3 Security claims

- An external attacker without a valid OIDC session or service credential
  cannot use the relay API.
- A principal cannot cross a team boundary through object identifiers, search,
  events, errors, database queries, or cached state.
- A principal cannot exceed relay-side role and session ACL permissions.
- Every relay-path operation is bound to exactly one active `HostShareId`, its
  current revision, and a matching immutable session origin. The operation
  cannot combine capabilities from different shares or exceed that share's
  local ceiling.
- A relay cannot enumerate or control local/direct-NetBird sessions.
- A remote caller cannot replace a host-authored profile or inject executable,
  environment, arbitrary path, mount, runtime, or secret configuration.
- A lost or revoked relay connection does not terminate host sessions or owner
  access.
- PTY bytes, input, prompts, file contents, credential material, and raw tokens
  do not enter PostgreSQL, audit records, structured logs, metrics, or traces.

### 8.4 Explicit non-claims

- A compromised relay may impersonate any principal and exercise every active
  `HostShare` it can reach.
- Host shares reduce relay authority but do not provide cryptographic separation
  between teams from a compromised relay holding all share state.
- Direct-host agent profiles run under the daemon owner's Unix account and are
  not a hostile-workload sandbox.
- A host owner or same-UID process can read and control all host sessions.
- The relay necessarily handles transient plaintext for terminal streams that it
  is authorized to proxy.
- PostgreSQL and relay backups contain identity, authorization, catalog, and
  audit metadata, although never terminal contents.

### 8.5 Security invariant enforcement matrix

| Invariant | Enforcing component | Required evidence |
|---|---|---|
| Only authenticated principals use the public API. | Relay OIDC/session and service-credential middleware | Authentication state-machine and credential rotation/revocation tests in [#85](https://github.com/zajca/pohunek/issues/85). |
| Object IDs, errors, search, events, and caches never cross a team boundary. | Relay authorization/query layer and PostgreSQL constraints | Cross-team guessed-ID and differential error/event tests in [#85](https://github.com/zajca/pohunek/issues/85) and [#83](https://github.com/zajca/pohunek/issues/83). |
| One operation cannot compose authority from multiple shares. | Relay router binds one share/revision; daemon re-resolves that exact local share | Per-method guard matrix and conflicting-share adversarial tests in [#70](https://github.com/zajca/pohunek/issues/70) and [#82](https://github.com/zajca/pohunek/issues/82). |
| Local and direct-overlay sessions are unreachable through the relay. | Daemon immutable `SessionOrigin` guard | Migration, forged metadata, guessed-ID, list, attach, and mutation tests in [#70](https://github.com/zajca/pohunek/issues/70). |
| Relay callers select only owner-authored profiles, registered projects, and bounded safe parameters. | Daemon profile/project/share-policy validation | Allowlist, canonical-path, parameter, and resource-limit tests in [#82](https://github.com/zajca/pohunek/issues/82). |
| The relay never initiates a host connection. | Host connector and relay listener topology | Network-direction and no-host-listener integration tests in [#72](https://github.com/zajca/pohunek/issues/72). |
| Ownership changes require exact local confirmation. | Daemon owner record and host approval signing key; relay conditional projection transaction | Replay, wrong-target, wrong-relay, stale-revision, crash, retry, and split-brain tests in [#81](https://github.com/zajca/pohunek/issues/81). |
| A gap or daemon epoch change cannot leave an apparently current catalog. | Relay host-link state machine | Concurrent snapshot, overflow, reorder, duplicate, restart, and full-resync tests in [#84](https://github.com/zajca/pohunek/issues/84). |
| Revocation cannot regain access to old sessions. | Daemon terminal share state and immutable, never-reused `HostShareId` | Suspend/reactivate and revoke/reapprove tests in [#82](https://github.com/zajca/pohunek/issues/82) and [#83](https://github.com/zajca/pohunek/issues/83). |
| Terminal content and secret material never become durable relay data or telemetry. | Relay schema, redacting types, audit/logging APIs, and attach proxy | Schema inspection, sentinel leakage, log/audit, crash, and backup tests in [#71](https://github.com/zajca/pohunek/issues/71) and [#87](https://github.com/zajca/pohunek/issues/87). |
| Relay loss or revocation never kills host sessions or removes owner access. | Daemon and session worker lifecycle boundary | Disconnect, unenrollment, revocation, restart, and owner-attach tests in [#70](https://github.com/zajca/pohunek/issues/70) and [#87](https://github.com/zajca/pohunek/issues/87). |

## 9. Host identity, ownership, and enrollment

### 9.1 Stable host identity

`pohunekd` generates one random stable `HostId` and stores it in an owner-only,
atomic, symlink-safe file. It is distinct from the daemon instance ID, worker
ID, runtime ID, hostname, WireGuard public key, and tunnel address.

A host has exactly one owner:

```text
HostOwner = PrincipalId | TeamId
```

The daemon stores the authoritative host owner record as the enrolled
`RelayId`, owner kind, opaque owner ID, and monotonically increasing revision.
PostgreSQL contains a routing and authorization projection, never an
independent owner authority. `pohunekd` does not resolve the principal, team, or
membership behind the opaque ID.

The enrolling principal is the initial owner unless enrollment explicitly names
a team the principal is permitted to bind. The local host operator is distinct
from that registered owner: same-UID Unix-socket access permits the operator to
approve a governance change, but does not make the operator a relay principal.
There are no co-owners.

Ownership transfer uses this retry-safe transaction:

1. The relay validates and temporarily reserves the exact target principal or
   team, then creates a short-lived one-use proposal bound to `RelayId`,
   `HostId`, current owner revision, target kind and ID, and a random nonce.
2. The daemon retrieves the proposal and displays the relay-resolved target plus
   immutable IDs. The local host operator explicitly confirms that exact
   proposal; neither the old nor new relay-side owner supplies another approval.
3. The daemon atomically persists the new owner revision, the consumed proposal
   outcome, and suspension of every active share. It then signs the complete
   confirmation with a host approval key generated and retained beside the
   enrollment key material.
4. The relay conditionally installs the projection only for the reserved target
   and previous revision. Duplicate delivery of the same signed outcome is
   idempotent; a changed target, nonce, relay, host, or revision fails closed.
5. If either process crashes after the daemon commit, reconnect reconciliation
   replays only that durable signed outcome. The daemon record wins; a missing,
   conflicting, or unverifiable relay projection quarantines governance and
   share mutations until repaired, without affecting owner-path sessions.

Shares do not silently transfer governance. They remain suspended after owner
transfer and require a new explicit local activation under the new owner.

### 9.2 Enrollment flow

1. The local host operator runs an interactive enrollment command handled by
   the existing `pohunekd` binary and authenticates the initial registered owner.
2. The daemon starts OIDC device authorization. If the configured issuer lacks
   device flow, it uses an exact loopback callback with PKCE, state, nonce, short
   deadlines, and one-use callback state.
3. The host generates its WireGuard private key locally. The private key never
   leaves the host and never appears in argv, JSON output, logs, errors, or
   debug formatting.
4. Over authenticated HTTPS, the daemon submits `HostId`, the WireGuard public
   key, bounded display metadata, ownership choice, and a one-use enrollment
   transaction identifier.
5. The relay atomically registers the host, assigns a unique tunnel IPv4
   address, and returns `RelayId`, relay WireGuard public key, UDP endpoint,
   relay tunnel address, and connection policy.
6. The daemon verifies and stores the enrollment in owner-only local state.
   There can be only one active relay enrollment.
7. The daemon initiates the WireGuard handshake and then the host-link TCP
   connection through the userspace stack.

Concurrent, replayed, expired, conflicting, and cloned-host enrollment fails
closed. A new enrollment cannot silently replace an existing one. Rotation uses
a separate locally confirmed transaction and a bounded overlap; private keys
remain host-generated.

### 9.3 Unenrollment

Local unenrollment immediately disables relay networking, invalidates all local
shares, closes host-link and attach streams, and makes relay-created sessions
owner-only without stopping them. Relay-side deletion prevents future
connections but cannot promise deletion of the host's local key; the host UI
must show both sides of the state.

## 10. Userspace WireGuard and network stack

### 10.1 Implementation

Both `pohunekd` and `pohunek-relayd` embed:

- a pinned released `boringtun` library for NoiseIK handshakes and WireGuard
  packet encryption/decryption; and
- a pinned `smoltcp` stack for bounded IPv4 and TCP sockets over decrypted
  packets.

The implementation does not use BoringTun's TUN-oriented CLI, a kernel network
interface, system routes, `wg`, `wg-quick`, or a privileged helper. BoringTun's
library deliberately supplies the WireGuard protocol but not the network stack;
the Pohunek adapter joins it to smoltcp.

The dependency choice is guarded by:

- exact versions in `Cargo.lock` and the repository dependency policy;
- known-answer and cross-implementation WireGuard tests;
- fuzz/property tests for packet and state-machine boundaries;
- `cargo audit`, feature-powerset clippy, and license review;
- explicit review of each dependency upgrade; and
- a transport trait that isolates crypto/stack details without pretending a
  different implementation is already supported.

### 10.2 Addressing and routing

- Each relay owns one configured private IPv4 pool used only inside its process.
- The relay tunnel address is fixed for a `RelayId`.
- Each enrolled host receives one unique address. Allocation, reuse cooldown,
  exhaustion, and release are transactional in PostgreSQL.
- The only permitted host route is the relay tunnel address. Hosts are not a
  routed mesh and cannot reach each other through this tunnel.
- The relay accepts packets only when UDP peer key, assigned source address,
  `HostId`, and active enrollment agree.
- Tunnel addresses are identifiers for this transport, never authorization by
  themselves.

### 10.3 Resource bounds

Packet sizes, fragment behavior, handshake rates, peers, TCP sockets, socket
buffers, retransmission queues, keepalive cadence, idle deadlines, and reconnect
backoff are configurable and have hard maxima. IPv4 fragmentation is rejected
unless the implementation can bound and test reassembly. The application does
not expose general UDP or arbitrary TCP through the tunnel.

## 11. Host-initiated application transport

### 11.1 Control link

After WireGuard becomes usable, `pohunekd` opens a long-lived TCP connection to
the fixed relay tunnel address. The host sends one bounded `HostLinkOpen`
prelude containing protocol range, `HostId`, enrollment identifier, daemon
instance identity, and a fresh connection nonce. The relay binds the stream to
the WireGuard peer and replies with the selected version and relay connection
identity.

After the prelude, the relay is the request initiator and `pohunekd` serves the
same typed daemon methods and event envelopes used by existing clients. The
connection's immutable authorization context is the enrolled relay, not an end
user. Relay request IDs are namespaced by the host-link connection and are never
accepted as globally unique.

Protocol v4 introduces the host-link prelude, relay-only methods, immutable
session origin, share coordinates, atomic share snapshot, event watermark, and
host-initiated attach negotiation. This is a coordinated cutover across Rust
and TypeScript protocol artifacts. There is no relay-path downgrade to v3.

Local Unix and direct overlay connections retain their existing owner context.
They do not require OIDC or relay credentials. Connection origin is derived from
the listener/link that accepted the request and cannot be selected in request
JSON.

### 11.2 Attach streams

The relay never opens a TCP connection to a host. To attach:

1. The client authenticates to the relay and passes relay-side ACL checks.
2. The relay sends an attach request on the host control link for an eligible
   relay-created session.
3. `pohunekd` rechecks the session origin and current `HostShare` capabilities,
   acquires a worker attach, and creates a cryptographically random one-use
   stream token.
4. `pohunekd` opens a new outbound TCP connection to the relay tunnel address
   and sends a bounded data-stream prelude containing the control connection
   identity and token.
5. The relay atomically pairs that stream with exactly one authorized client
   stream and proxies opaque bytes with bounded buffers and backpressure.

Tokens expire quickly, are bound to one host link, session, runtime generation,
and operation, and are consumed once. Half-close, disconnect, cancellation,
share revocation, ACL revocation, and relay shutdown have explicit tested
behavior. Terminal bytes are never logged or persisted.

## 12. `HostShare` authorization

### 12.1 Creation and approval

A team administrator may request access to a host. A request grants nothing.
The local host operator must approve it through the local daemon interface on
behalf of the registered owner. Approval creates an opaque stable `HostShareId`
bound to exactly one team and one relay enrollment. A `HostShareId` is never
recycled or rebound.

The daemon persists the authoritative share. The relay stores a projection for
routing and user authorization. Relay state never expands the local share.

### 12.2 Share policy

Each share contains a revisioned, default-deny policy:

- allowed daemon operation classes;
- allowed owner-authored agent profile names;
- allowed registered project IDs and canonical worktree roots;
- allowed safe launch parameters and metadata key namespace;
- whether terminal observation, interactive control, lifecycle control, fork,
  resume, rename, sharing metadata, and removal are available;
- per-share concurrent and retained session limits;
- per-share attach, waiter, subscription, request, bandwidth, and buffer limits;
  and
- activation, expiry, reversible suspension, and terminal revocation state.

The relay carries `HostShareId` on every relay-path request. The daemon resolves
typed target IDs and intersects the operation with the current local share. It
never trusts a relay-supplied path, executable, profile body, role, capability
set, or policy revision.

The daemon does not inspect user, group, role, or session ACL claims. Optional
`created_by_principal`, `created_for_team`, and request correlation fields are
bounded opaque attribution metadata and are never authorization inputs.

### 12.3 Session origin

Every session has immutable typed origin:

```text
SessionOrigin = LocalOwner
              | DirectOverlayOwner
              | Relay { relay_id, host_share_id }
```

Because a revoked `HostShareId` is terminal and never reused, a session origin
cannot regain relay eligibility through later approval of a similarly named
share.

Existing sessions migrate to an owner origin based on durable creation data;
ambiguous legacy sessions fail safe as owner-only. Free-form metadata cannot
set or change origin.

Relay requests can list, inspect, observe, attach, mutate, fork, resume, stop,
rename, share, or remove only sessions whose origin matches the active relay
enrollment and supplied active share. A relay-created fork keeps the same
origin. A relay cannot use a relay-created session as a handle to reach another
local resource outside the share.

Owner paths retain full authority over every session, including relay-created
sessions.

### 12.4 Revocation

Local share suspension or terminal revocation immediately:

- rejects new relay operations;
- closes affected control-derived waits, subscriptions, and attach streams;
- excludes the sessions from later relay snapshots; and
- emits only a bounded revocation acknowledgement to the relay.

It does not stop or remove sessions. They continue under host authority and are
available only through owner paths. A suspended share may be reactivated at a
new revision, making its existing relay-origin sessions eligible again. A
revoked share can never be reactivated: later approval creates a new
`HostShareId`, and sessions carrying the revoked ID remain permanently
owner-only. Republishing them under a new share is not supported by this RFC.

## 13. Relay identity and authorization

### 13.1 Human authentication

Browser login uses OIDC Authorization Code with PKCE. CLI login and host
enrollment use device authorization when supported, otherwise an exact loopback
browser callback. All flows validate issuer, audience, redirect URI, state,
nonce, PKCE verifier, code replay, clock bounds, and stable subject.

Browser sessions are server-side, rotating, revocable, idle-bounded, and backed
by Secure, HttpOnly, SameSite cookies. Mutations require CSRF protection.
WebSocket upgrades require authentication and an exact Origin allowlist before
any target lookup.

### 13.2 Service accounts

A service account is a first-class principal. It authenticates with a random
high-entropy credential consisting of a public credential ID and secret. The
relay stores only a keyed digest, state, timestamps, and audit metadata.
Credentials have mandatory expiry, rotation overlap, last-used tracking, and
immediate revocation. Raw credentials are shown once, never accepted in URLs or
argv, and never logged.

Credentials carry no embedded authorization scope. Current team membership,
roles, and grants are resolved server-side on every authorization decision so
revocation does not wait for credential expiry.

### 13.3 Teams, groups, and roles

The relay provides built-in `Owner`, `Admin`, and `Member` roles plus custom
roles composed from stable permissions. Grants can target principals, service
accounts, and groups, and can be narrowed to teams, host shares, projects, and
sessions.

Every database lookup is team-scoped before authorization. Display names are
never identity. Deprovisioned principals and deleted groups lose new access
immediately; long-lived connections are cancelled according to the same policy
revision.

Relay infrastructure administration is separate from team administration. An
infrastructure administrator has no implicit application permission to read or
control sessions. This is an application RBAC guarantee, not protection from a
trusted operator with process or database access.

## 14. Session visibility and sharing

Relay-created sessions belong to a team and record the creating principal for
attribution. They are team resources rather than host-daemon user objects.

Every active team member can see a minimal existence record containing an
opaque relay session reference, its host-share display reference, and a coarse
available/unavailable state. The existence record excludes session title,
creator, profile, project, branch, worktree, metadata, timestamps, terminal
state, notifications, and failure details.

The creator receives full relay-side session permissions by default. Any access
beyond existence requires an explicit session grant, a matching custom role, or
the built-in team `Owner`/`Admin` authority. Permission classes are separate:

- `session.metadata.read`;
- `session.terminal.observe`;
- `session.terminal.control`;
- `session.lifecycle.control`;
- `session.share.manage`; and
- `session.remove`.

Interactive attach always requires terminal control because client bytes can
reach the PTY. Read-only users use bounded screen/output observation. Team
owners and administrators may inspect, control, share, stop, and recover team
sessions. Host owners may control every relay-created host session through an
owner path and, when their principal/team identity is authorized by the relay,
through the relay API.

ACL changes are atomic, revisioned, audited, and cancel affected live access.
The relay applies identical filtering to lists, direct lookups, events, search,
errors, and notifications.

## 15. Relay architecture

### 15.1 Process and crates

The Cargo workspace adds a `pohunek-relay` library crate and a thin
`pohunek-relayd` binary. Internal modules separate configuration, PostgreSQL
repositories and migrations, OIDC, browser sessions, service credentials,
authorization, team administration, host enrollment, WireGuard, host links,
state synchronization, routing, attach proxying, audit, quotas, HTTP/WebSocket
ingress, health, and shutdown.

Shared host-link types live in `crates/protocol`. Relay-client API types live in
a dedicated Rust crate and generate the TypeScript contract consumed by the CLI
and web workspace. Browser clients never send arbitrary daemon NDJSON through a
transparent tunnel.

### 15.2 PostgreSQL

PostgreSQL is required. Versioned transactional migrations cover:

- relay identity and configuration revision;
- OIDC identities and account links;
- principals, teams, memberships, groups, built-in/custom roles, and grants;
- browser sessions and service credential digests;
- hosts, ownership, enrollments, WireGuard peer keys, tunnel addresses, and
  connection state;
- share requests and accepted `HostShare` projections;
- relay-created session catalog and ACLs;
- policy/revocation generations;
- audit records; and
- quota/accounting state that must survive restart.

Database constraints enforce team coordinates and prevent cross-team foreign
keys, duplicate host identity, duplicate tunnel addresses, duplicate active
enrollment, stale revision writes, and last-owner deletion. Authorization is
also enforced in the service layer; row scoping is not left to UI filtering.

The relay may persist reconstructible session catalog metadata and the last
known connection state, marked stale after disconnect. It never persists
terminal screen snapshots, output, input, prompts, file contents, profile
environment, or attach buffers.

### 15.3 Public API

`pohunek-relayd` exposes:

- HTTPS JSON endpoints for login, account, team, role, service account, host,
  ownership, share request, session ACL, audit, and administrative mutations;
- an authenticated, versioned WebSocket control/event API for host and session
  operations and catalog updates;
- a dedicated authenticated binary WebSocket per attach stream; and
- liveness and readiness endpoints that reveal no tenant data.

The relay API uses stable typed errors, opaque IDs, idempotency keys for
mutations, correlation IDs, pagination, optimistic revision checks, and bounded
request/response sizes. Clients select relay mode explicitly. Failure never
falls back to a direct owner connection under different credentials.

TLS termination is mandatory. The relay can terminate TLS with configured
rustls certificate/key paths or trust a specifically configured loopback
reverse proxy that supplies no identity headers. Required public origin, OIDC,
PostgreSQL, TLS mode, WireGuard endpoint, address pool, limits, and key material
fail fast when absent or invalid.

## 16. State synchronization

### 16.1 Contract

Only relay-created sessions for active shares are synchronized. Each host link
has exactly one host-scoped subscription with a random daemon-boot epoch and a
strictly increasing canonical decimal sequence shared by all of that host's
shares. The daemon retains no replay window for this feature.

The protocol provides an atomic host-scoped snapshot operation returning:

- host and daemon identities;
- active share revisions visible to the enrolled relay;
- the complete eligible session/notification projection;
- the subscription epoch; and
- a watermark sequence included in that snapshot.

Every later event contains the same epoch and a higher sequence.

### 16.2 Subscription-first algorithm

1. The relay establishes the host link and starts a bounded subscription.
2. The daemon registers the subscriber before capturing the snapshot.
3. Events arriving during snapshot construction enter the bounded subscriber
   queue.
4. The daemon returns the atomic snapshot and watermark.
5. The relay installs the snapshot, discards buffered events at or below the
   watermark, and applies higher sequences in exact order.
6. Duplicate events are ignored only when their epoch and sequence are already
   committed.
7. Queue overflow, sequence gap, wrong order, epoch change, malformed event, or
   host-link loss marks the host degraded, discards its live cache, and repeats
   the entire bounded procedure.

The relay never continues from a suspected gap and never asks the daemon for
replay. PostgreSQL may retain the last catalog as explicitly stale UI data, but
it is not used for authorization or mutation routing until a current snapshot
is installed.

## 17. Audit, logging, and data retention

### 17.1 Audit policy

Audit records contain only structured metadata: actor, actor type, team,
`HostId`, `HostShareId`, session reference, action, decision, policy revision,
correlation/request ID, safe bounded parameters, timestamp, and outcome.

Authentication, membership, role, credential, enrollment, ownership, share,
ACL, lifecycle, terminal-access open/close, revocation, and administrative
changes are audited. Sensitive access is not granted if its required audit
decision cannot be durably recorded.

Audit excludes raw tokens, cookie values, OIDC codes, WireGuard key material,
profile environment, prompts, input, PTY output, terminal snapshots, file
contents, provider bodies, and arbitrary error payloads.

### 17.2 Data classification

| Data | Host persistence | Relay persistence | Logs/audit | Retention |
|---|---|---|---|---|
| Host/relay public identity | Yes | Yes | IDs only | Until deletion plus audit policy |
| Host WireGuard private key | Host only | Never | Never | Until rotation/unenrollment |
| Relay WireGuard private key | Never | Protected relay secret storage | Never | Until rotation/decommission |
| WireGuard public key/address | Yes | Yes | Bounded identifiers | Until cooldown/audit expiry |
| Human/team/role metadata | No | PostgreSQL | Bounded IDs/actions | Configured policy |
| Service credential secret | Client only after issuance | Keyed digest only | Never | Until expiry/revocation |
| `HostShare` policy | Authoritative | Projection | Revision and decision | Until deletion plus audit policy |
| Relay session catalog/ACL | Origin authoritative on host; ACL not stored | PostgreSQL | IDs/actions | Configured metadata policy |
| PTY output/input/snapshot | Existing bounded host runtime only | Never durable | Never | Existing host policy |
| Prompt/file/repository content | Existing host semantics | Never | Never | Existing host policy |
| Audit metadata | Optional local security event | PostgreSQL | The audit record | Configured append-only policy |
| Attach buffers | Memory only | Memory only | Never | Until forwarded/disconnected |

Structured process logs go to the repository-established runtime logging
destination and include correlation IDs, latency, sizes, and safe state
transitions. They do not duplicate audit or terminal content. Metrics avoid
unbounded labels and tenant-controlled strings.

## 18. Quotas and backpressure

The relay enforces independently configurable hard limits globally and per team,
principal, service account, host, share, and session where applicable:

- unauthenticated/authenticated connections and login attempts;
- HTTP requests, WebSocket connections, request body/response sizes, and rates;
- queued and concurrent host RPCs;
- host reconnect attempts and handshakes;
- subscriptions, waiters, attach streams, and attach bandwidth;
- in-memory catalog entries, event queues, packet buffers, TCP buffers, and
  attach buffers;
- database rows/bytes governed by product retention; and
- audit and asynchronous cancellation queues.

Backpressure propagates to the producing stream or closes it with a stable typed
overload error. No unbounded channel is permitted. One team, host, or client
cannot consume global capacity or affect local/NetBird owner access.

## 19. Failure and recovery semantics

| Failure | Required behavior |
|---|---|
| Relay unavailable | Local and NetBird modes continue; relay-created sessions keep running. |
| Host unavailable | Relay marks catalog stale and rejects mutations; other hosts continue. |
| Host reconnect | Full subscription-first snapshot; no replay assumption. |
| Daemon restart | Workers and sessions retain existing durability; new event epoch triggers full resync. |
| Relay restart | PostgreSQL restores auth/catalog metadata; every host must provide a fresh snapshot before mutation routing. |
| Event gap/lag | Discard affected live host cache and resnapshot. |
| Share suspended | Close affected relay access; sessions continue owner-only and leave the live catalog until explicit reactivation. |
| Share revoked | Close affected relay access permanently; the ID cannot be reused and sessions continue owner-only. |
| User/ACL revoked | Relay cancels that principal's active client streams; daemon need not know why. |
| WireGuard key compromised | Disable enrollment, close links, rotate locally, and require fresh binding. |
| Relay compromised | Revoke local enrollment/shares; assume every active share was exercisable and follow incident runbook. |
| Host identity clone | Quarantine conflicting connections; do not pick the newest implicitly. |
| PostgreSQL unavailable | Readiness fails; no new sensitive action without durable authorization/audit; host sessions continue. |
| Disk full/migration failure | Fail startup or mutation transaction safely without broadening access. |

Reconnect uses bounded exponential backoff with jitter and storm protection.
Shutdown stops new ingress, cancels login/enrollment transactions, drains or
fails bounded requests, closes streams, releases the active relay lease, and
exits within a configured deadline.

## 20. Deployment and operations

The supported first deployment is one active unprivileged `pohunek-relayd`
process, PostgreSQL, one public HTTPS endpoint, and one public WireGuard UDP
endpoint. It can run under systemd or in a container without `CAP_NET_ADMIN`.

Exactly one active process may own a `RelayId`; a PostgreSQL advisory/lease
record prevents accidental concurrent activation. Backup includes PostgreSQL
and separately protected relay secret/key material. Restore procedures detect
rollback of policy generations and duplicated active identity.

Readiness distinguishes PostgreSQL, migrations, relay identity, TLS, OIDC
configuration, WireGuard bind, audit durability, and host connectivity.
Individual offline hosts degrade readiness details but do not make the relay
process unready. Liveness does not depend on external IdP or hosts.

Runbooks cover enrollment, ownership transfer, share approval/revocation,
credential rotation, OIDC outage, host loss, relay loss, database backup/restore,
key compromise, identity clone, reconnect storm, event gap, disk exhaustion,
upgrade, rollback, and complete unenrollment.

## 21. Upgrade and rollback

Protocol v4 is a coordinated pre-1.0 cutover. All Rust and TypeScript protocol
artifacts, daemon, CLI, GUI, web packages, and tests update together. There is no
v3 relay-host compatibility shim.

Direct owner operation remains available throughout rollout:

1. Upgrade owner clients and `pohunekd` to the release supporting v4.
2. Deploy PostgreSQL and `pohunek-relayd`.
3. Enroll hosts one at a time; unenrolled hosts remain standalone/NetBird-only.
4. Create and locally approve shares.
5. Enable team clients only after the host reports a current snapshot.

Rolling back disables relay links and returns relay-created sessions to
owner-only operation without stopping them. Database rollback uses a documented
compatible backup or reversible migration boundary; application binaries never
silently run against a newer unsupported schema.

## 22. Testing strategy

### 22.1 Unit and property tests

- Host identity, ownership, exact signed transfer transactions, enrollment,
  rotation, clone detection, share revision, scope resolution, origin migration,
  suspension/reactivation, and terminal revocation with never-reused IDs.
- WireGuard packet/handshake known-answer tests and smoltcp buffer/state bounds.
- Protocol v4 prelude, version selection, typed relay methods, gap detection,
  snapshot watermark, attach tokens, framing, and size limits.
- Team-scoped repositories, roles, custom permissions, ACL evaluation, service
  credential digest/rotation, OIDC state machines, CSRF, Origin, and redaction.
- Every daemon relay method has an explicit `HostShare` operation mapping;
  adding a method without one fails a test.

### 22.2 Integration and adversarial tests

- Real `pohunek-relayd`, PostgreSQL, `pohunekd`, and session worker with the
  userspace WireGuard transport and no network capabilities.
- Standalone, NetBird-only, relay-only, and mixed NetBird-plus-relay topologies.
- Multiple hosts, teams, shares, users, groups, custom roles, and service
  accounts with cross-team/object-ID attacks.
- Proof that local/NetBird sessions never appear in snapshots or accept relay
  operations, including guessed IDs and crafted metadata.
- Host-initiated control and attach only; the relay cannot dial the host.
- Host-scoped multi-share snapshot concurrency, subscriber overflow,
  lost/reordered/duplicate events, daemon/relay restart, reconnect storm, and
  stale PostgreSQL catalog.
- OIDC callback replay, wrong issuer/audience/state/nonce/PKCE, account
  collision, cookie fixation, CSRF, WebSocket Origin, API-token leakage, and
  rate limits.
- Backpressure, half-close, cancellation, expiry, share/ACL revocation, memory
  limits, connection limits, and bandwidth limits.
- PostgreSQL migration, backup/restore, disk-full, read-only, corruption,
  transaction conflict, and relay identity duplication.

### 22.3 Required repository gates

Every implementation milestone runs the applicable gates from `AGENTS.md`.
The complete track must pass the full Rust gates, TypeScript generation check,
documentation/knowledge check, web typecheck/lint/unit/browser tests, real-daemon
suite, dependency audit, feature-powerset clippy, and release packaging tests.

## 23. Issue and dependency map

The implementation is delivered through the following GitHub issues. Each issue
body is normative for its bounded implementation scope; this RFC wins if an old
description conflicts until the issue is reconciled.

| Order | Issue | Outcome |
|---:|---|---|
| 1 | [#80](https://github.com/zajca/pohunek/issues/80) | Land this RFC and update canonical architecture, roadmap, agent guidance, and knowledge. |
| 2 | [#81](https://github.com/zajca/pohunek/issues/81) | Stable host identity, single relay enrollment state, exact owner, local ownership transfer. |
| 3 | [#85](https://github.com/zajca/pohunek/issues/85) | Rust relay foundation, PostgreSQL schema, OIDC, principals, teams, groups, roles, and service credentials. |
| 4 | [#72](https://github.com/zajca/pohunek/issues/72) | Embedded userspace WireGuard, address allocation, OIDC host enrollment, and host-initiated link transport. |
| 5 | [#70](https://github.com/zajca/pohunek/issues/70) | Protocol v4 host link, daemon relay context, immutable origin, and share guard framework. |
| 6 | [#82](https://github.com/zajca/pohunek/issues/82) | Locally approved `HostShare` lifecycle and profile/project/operation/resource policy. |
| 7 | [#83](https://github.com/zajca/pohunek/issues/83) | Relay-side session visibility, creator rights, ACLs, admin authority, and revocation. |
| 8 | [#84](https://github.com/zajca/pohunek/issues/84) | Subscription-first atomic snapshot/watermark and gap-triggered resync without replay. |
| 9 | [#71](https://github.com/zajca/pohunek/issues/71) | Relay host-link manager, router, state aggregator, attach proxy, and typed public API. |
| 10 | [#86](https://github.com/zajca/pohunek/issues/86) | Team CLI, Svelte web control surface, and removal of the production Bun backend authority. |
| 11 | [#87](https://github.com/zajca/pohunek/issues/87) | Audit, quotas, deployment, backup/restore, observability, and incident hardening. |
| Later | [#73](https://github.com/zajca/pohunek/issues/73) | Provider webhook delivery and encrypted token vault. |
| Later | [#88](https://github.com/zajca/pohunek/issues/88) | Real profile-backed container/VM runtime isolation. |

[#69](https://github.com/zajca/pohunek/issues/69) is complete and preserves the
generic direct-overlay architecture used by NetBird. It is a prerequisite fact,
not a reason to force the relay tunnel through the direct-overlay discovery API.

The intended dependency graph is:

```text
#80 -> #81, #85
#69 + #81 + #85 -> #72
#72 + #81 -> #70
#70 + #85 -> #82, #83
#70 + #82 -> #84
#70 + #72 + #82 + #83 + #84 + #85 -> #71
#71 -> #86
#71 + #72 + #85 -> #87
#71 + #84 + #85 + #87 -> #73
#82 -> #88
```

## 24. Definition of done

The team relay track is complete only when all of the following are true:

- all four supported topologies work and are covered by integration tests;
- standalone and NetBird-only behavior does not require relay configuration;
- one or more NetBird-connected hosts can independently join one relay;
- the relay never initiates a network connection to a host;
- neither daemon nor relay requires a kernel WireGuard interface or elevated
  network capability;
- a host has one relay enrollment and one principal-or-team owner, with locally
  confirmed transfer;
- team admins can request shares but a share is inert until local approval;
- daemon-side share enforcement blocks disallowed profile, project, operation,
  resource, and session-origin access;
- guessed IDs and crafted metadata cannot expose local/NetBird sessions;
- the relay handles human OIDC and expiring rotatable service credentials;
- built-in and custom roles work across principals, groups, shares, projects,
  and sessions without cross-team leakage;
- every member sees session existence while content/control follows ACLs and
  owner/admin authority;
- PostgreSQL migrations, transactions, backup, restore, and failure modes are
  tested;
- subscription-first snapshot synchronization recovers from all gaps without
  daemon replay;
- relay and daemon restarts preserve host PTYs and converge catalog state;
- revoking a share removes relay access without stopping its sessions;
- no forbidden terminal or secret data reaches relay persistence or telemetry;
- quotas and backpressure prevent one tenant or host from exhausting the relay;
- CLI and web provide the same typed permissions and failure behavior;
- the deployed runtime has exactly one auth/routing authority: the Rust relay;
- security and operational runbooks pass their automated checks; and
- every required repository, web, protocol, docs, security, and release gate is
  green.

## 25. Rejected alternatives

### 25.1 Replace NetBird with the relay

Rejected. The relay is optional and cannot become a dependency for established
owner workflows. Direct NetBird remains useful for multiple owner machines and
continues alongside relay enrollment.

### 25.2 Relay dials daemon listeners

Rejected. It requires host reachability, route management, or placement inside
the host mesh. Hosts instead establish all connections to the public relay.

### 25.3 Separate connector sidecar

Rejected. Enrollment, host policy, session origin, and PTY routing already
belong to `pohunekd`; a sidecar would duplicate authority and lifecycle.

### 25.4 Kernel WireGuard or privileged helper

Rejected. The relay tunnel carries only Pohunek application traffic and can be
implemented with an in-process WireGuard protocol plus bounded TCP/IP stack.
Kernel routes and `CAP_NET_ADMIN` would unnecessarily expand the deployment and
privilege boundary.

### 25.5 Per-user grants validated by `pohunekd`

Rejected. User, group, role, and ACL logic belongs to the relay. Duplicating it
in the daemon creates two authorization authorities. The daemon instead
enforces an enrolled-relay context plus local `HostShare` ceilings.

### 25.6 Relay access to all host sessions

Rejected. A relay credential must not turn existing local or NetBird work into
team-visible state. Immutable session origin is enforced by the host.

### 25.7 Event replay log in the daemon

Rejected for the relay contract. Subscription-first atomic snapshotting avoids
a persistent replay subsystem while still preventing lost startup mutations.
Any detectable uncertainty causes a full bounded resynchronization.

### 25.8 Continue the Bun backend as relay authority

Rejected. Authentication, WireGuard, routing, aggregation, audit, and quotas
must have one implementation authority. Bun remains for frontend build/test
tooling; the production relay is Rust.

### 25.9 SQLite relay storage

Rejected. The public multi-team service needs transactional concurrency,
operational backups, migrations, and future active-passive options. PostgreSQL
is required from the first release.

### 25.10 Persist terminal output for reconnect

Rejected. Host workers already own bounded terminal state. The relay requests
fresh bounded observations after reconnect and never becomes a terminal archive.

## 26. External implementation references

- [BoringTun 0.7.1 documentation](https://docs.rs/boringtun/0.7.1/boringtun/)
  and [crate metadata](https://crates.io/crates/boringtun/0.7.1) are the fixed
  baseline evaluated by this RFC, not an automatic implementation choice.
  Before [#72](https://github.com/zajca/pohunek/issues/72) pins any version, its
  security record must explicitly dispose of the upstream
  [packet-padding defect](https://github.com/cloudflare/boringtun/issues/494)
  and [0.7.1 connectivity regression](https://github.com/cloudflare/boringtun/issues/495),
  plus any newer relevant reports. An affected release is unacceptable without
  a reviewed project-owned patch and regression evidence.
- [smoltcp 0.14.0 documentation](https://docs.rs/smoltcp/0.14.0/smoltcp/) and
  [crate metadata](https://crates.io/crates/smoltcp/0.14.0) are the fixed
  evaluation baseline for bounded userspace IPv4/TCP. Issue #72 must still
  confirm Pohunek's MSRV, feature set, memory bounds, and adversarial packet
  behavior before pinning it.
- [WireGuard's project list](https://www.wireguard.com/repositories/) records the
  upstream status of WireGuard implementations used during dependency review.
