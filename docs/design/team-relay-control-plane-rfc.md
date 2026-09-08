# RFC: Optional Team Relay Control Plane

- Status: Accepted for implementation
- Date: 2026-09-02
- Tracking issue: [#80](https://github.com/zajca/pohunek/issues/80)
- Reconciliation: [#91](https://github.com/zajca/pohunek/issues/91), 2026-09-08, pending merge
- Umbrella issue: [#56](https://github.com/zajca/pohunek/issues/56)
- Related issues: [#69](https://github.com/zajca/pohunek/issues/69), [#70](https://github.com/zajca/pohunek/issues/70), [#71](https://github.com/zajca/pohunek/issues/71), [#72](https://github.com/zajca/pohunek/issues/72), [#73](https://github.com/zajca/pohunek/issues/73), [#80](https://github.com/zajca/pohunek/issues/80), [#81](https://github.com/zajca/pohunek/issues/81), [#82](https://github.com/zajca/pohunek/issues/82), [#83](https://github.com/zajca/pohunek/issues/83), [#84](https://github.com/zajca/pohunek/issues/84), [#85](https://github.com/zajca/pohunek/issues/85), [#86](https://github.com/zajca/pohunek/issues/86), [#87](https://github.com/zajca/pohunek/issues/87), [#88](https://github.com/zajca/pohunek/issues/88), [#91](https://github.com/zajca/pohunek/issues/91), and [#92](https://github.com/zajca/pohunek/issues/92)

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

The shipped owner WebUI remains a separate supported path. Its Bun backend runs
inside the owner trust domain, discovers the local daemon and direct-overlay
peers, and transparently maps browser WebSockets to daemon connections. The
relay neither replaces that backend nor needs a local mode. Owner and team web
surfaces may share presentation code, but their transports, credentials, state,
and origins remain explicit and fail closed without cross-mode fallback.

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
9. Browser login uses OIDC Authorization Code with PKCE. Human CLI login and
   host enrollment use mandatory OIDC device authorization; there is no
   loopback browser callback fallback. WireGuard private keys are generated and
   retained on the host.
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
18. The existing owner WebUI and `web/backend` remain supported for local and
    direct-overlay access. `pohunek-relayd` has no owner/local mode. Team and
    owner browser modes use separate explicit API adapters and credentials,
    even when they share Svelte presentation components.

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
- Replacing the existing owner WebUI or moving its Unix/NetBird gateway into
  `pohunek-relayd`.
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
| `web/frontend` | Owner-mode browser presentation and reusable Svelte presentation components for the future team surface | Authorization authority, cross-mode fallback, durable terminal data |
| `web/backend` | Supported owner-mode host discovery, SPA serving, and transparent one-WebSocket-to-one-daemon tunneling over local/direct-overlay paths | Relay routing, relay auth, team aggregation, or public-Internet exposure |

`pohunek-relayd` serves the team-mode SPA and its authenticated typed
HTTP/WebSocket API. The separately deployed Bun backend remains the owner-mode
WebUI gateway and continues to serve the owner SPA. It has no relay authority,
and the Rust relay has no local/owner mode. The two surfaces may reuse Svelte
components and framework-independent presentation helpers, but not transport,
credential, authorization, or session-state adapters.

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

Owner WebUI (independent of relay)
owner browser -> private HTTP/WSS -> web/backend
web/backend -> local Unix socket ------------------+-> pohunekd
            -> direct overlay/NetBird TCP ---------+
```

The owner WebUI keeps the shipped transparent daemon protocol and `/api/hosts`
catalog. It does not accept relay credentials or expose relay-only team state.
The team WebUI uses only the typed relay API and does not fall back to the owner
gateway when the relay, login, team, share, or host is unavailable. Each origin
declares exactly one mode; a deployment may expose both on different origins.

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

Each row names the sole linearization state where one exists; references name
the normative algorithm rather than introducing another one.

| Invariant | Enforcer | Durable state / linearization | Failure behavior | Deterministic adversarial test | Owner |
|---|---|---|---|---|---|
| Only authenticated, current principals use the public API. | Relay OIDC/session and service-credential middleware. | #85 credential/session generation and current authorization decision, committed with its audit record. | Deny sensitive admission when decision or audit persistence is unavailable; revoke still closes access. | Replay a rotated/revoked credential, lost audit write, stale recovery generation, and cookie/bearer mix-up. | [#85](https://github.com/zajca/pohunek/issues/85), [#71](https://github.com/zajca/pohunek/issues/71) |
| IDs, errors, search, events, caches, and multipart parts do not cross teams. | Relay authorization/query layer and PostgreSQL constraints. | Team-scoped row keys and the projection transaction in §16.2. | Return typed non-disclosure denial; discard an unauthorized cache/part. | Guess cross-team IDs and substitute a valid part, error, event, or cache row from another team. | [#83](https://github.com/zajca/pohunek/issues/83), [#84](https://github.com/zajca/pohunek/issues/84) |
| One operation cannot compose authority from shares. | Relay router and daemon exact-share re-resolution. | Ticket/share revision and host journal CAS in §12.5. | Reject missing, stale, foreign, mixed, or changed-share coordinates. | Race conflicting shares and retry one ticket with a changed share or payload. | [#70](https://github.com/zajca/pohunek/issues/70), [#82](https://github.com/zajca/pohunek/issues/82) |
| Relay cannot reach local/direct-overlay sessions or create authority from observed resources. | Immutable `SessionOrigin` and `ResourceBindingV1` checks. | Host create transaction and journal resource-prepared/commit states in §12.5. | Typed denial before worker registration; recover only recorded resources. | Forge origin/cwd, swap a symlink, remove a profile, and race fork/resume/native recovery. | [#70](https://github.com/zajca/pohunek/issues/70), [#82](https://github.com/zajca/pohunek/issues/82) |
| A caller selects only owner-approved profiles, projects, safe parameters, and bounded resources. | Daemon profile/project/share-policy validation. | Locally approved share revision and immutable resource binding. | Reject unavailable revisions, path escape, unsafe parameters, or exhausted bound. | Use stale approval, transitive diff source, parameter overflow, and canonical-path escape. | [#82](https://github.com/zajca/pohunek/issues/82) |
| The relay never dials a host. | Host connector and relay listener topology. | Active enrollment binds peer key/address/HostId; no relay host dial state exists. | Reject inbound host-facing route or mismatched peer binding. | Observe all socket opens while attempting relay-initiated control and attach connections. | [#72](https://github.com/zajca/pohunek/issues/72) |
| Exact local confirmation fences ownership/governance changes. | Daemon owner record, host approval key, relay conditional transaction. | Daemon owner revision is authoritative; relay projection conditional on it (§9.1). | Quarantine governance/share mutations on missing, conflicting, or unverifiable projection. | Replay/wrong target, relay, nonce, revision, crash, retry, and split-brain confirmation. | [#81](https://github.com/zajca/pohunek/issues/81) |
| Catalog, notification, and share projection never appear current across a gap, epoch, or incomplete multipart snapshot. | Daemon projection coordinator and relay host-link state machine. | One coordinator sequence; atomic relay manifest/parts/current-watermark transaction (§16.2). | Mark degraded, discard live cache, and resnapshot; never route by suspect state. | Reorder/duplicate/drop events, overflow queue, mutate notification during freeze, corrupt/miss a part, and crash the install transaction. | [#84](https://github.com/zajca/pohunek/issues/84) |
| Journal/ticket replay cannot create a second mutation or turn uncertain terminal input into a retry. | Daemon ticket issuer and operation journal. | Ticket nonce/fingerprint and irreversible-commit CAS (§12.5). | Return recorded safe result/in-progress; reject compacted ticket; input disconnect is unknown and never replayed. | Lose issue/begin/result ACKs, crash every journal stage, race cancellation/revocation, and alter input bytes. | [#82](https://github.com/zajca/pohunek/issues/82) |
| Revocation, evidence expiry, deletion suppression, and restore quarantine cannot resurrect old access/catalog state. | Relay cancellation registry, daemon terminal share state, recovery-generation gate, catalog suppression. | Never-reused `HostShareId`; decision/recovery generation; retirement checkpoint and compacted floor (§§13.1--13.6, 17.3, 19.1). | Close affected streams; quarantine restore; fence old host before publication; host PTY continues. | Reapprove after revoke, restore pre-revocation backup, reconnect old host backup, and lose a suppression ACK. | [#85](https://github.com/zajca/pohunek/issues/85), [#83](https://github.com/zajca/pohunek/issues/83), [#84](https://github.com/zajca/pohunek/issues/84), [#87](https://github.com/zajca/pohunek/issues/87) |
| Terminal content, credentials, and raw tokens never become durable relay data or telemetry. | Schema, redacting types, audit/logging APIs, attach proxy. | Schema permits only metadata/digests; no terminal-content persistence point exists. | Redact/reject unsafe fields and fail closed when required audit cannot persist. | Inject content/secret sentinels through attach, error, crash, backup, log, audit, metric, and trace paths. | [#71](https://github.com/zajca/pohunek/issues/71), [#87](https://github.com/zajca/pohunek/issues/87) |
| Relay loss, revocation, quota pressure, or multiwriter contention does not kill a host session or deny reserved owner control under the supported profile. | Daemon/worker lifecycle boundary and relay fair scheduler. | Per-share queue accounting plus reserved control slots in §18; worker input ordering per stream (§11.3). | Close/overload only producing relay work; owner inspect/stop/wait consumes reserved control capacity. | Saturate one share, use multiple writers/resizes, disconnect/revoke relay, then exercise owner controls. | [#70](https://github.com/zajca/pohunek/issues/70), [#71](https://github.com/zajca/pohunek/issues/71), [#87](https://github.com/zajca/pohunek/issues/87) |
| Webhook observation gaps never impersonate durable delivery; snapshots never claim offline terminal history. | #73 webhook ingest/outbox and §16 projection consumer. | Source observation cursor is separate from admitted-event outbox sequence; snapshots use only frozen current projection. | Mark source gap/unknown and require reconciliation; do not synthesize history. | Drop/reorder upstream webhook before admission, duplicate after outbox commit, then compare cursor/outbox/projection. | [#73](https://github.com/zajca/pohunek/issues/73), [#84](https://github.com/zajca/pohunek/issues/84) |

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
2. The daemon starts mandatory OIDC device authorization and binds the successful
   authentication to the exact one-use enrollment transaction and host proof. If
   device authorization is unavailable, enrollment fails clearly; it never falls
   back to a loopback callback.
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

The following is an **evaluation-only, proposed and unmeasured** reference
profile. It is not a Cargo edit, a selected production dependency, or evidence
that a relay transport works: `boringtun = 0.7.1`, with
`default-features = false` and no `device`, `ffi-bindings`, or `jni-bindings`;
and `smoltcp = 0.14.0`, with `default-features = false` and only `std`,
`medium-ip`, `proto-ipv4`, and `socket-tcp`. The resulting Cargo.lock must use
the published crate checksums, record the complete resolved feature graph and
license/advisory review, and build with the repository MSRV Rust 1.96. The
evaluation record includes the exact crate archive SHA-256, Cargo.lock package
checksum, rustc version/target, and upstream source revision for both packages.
It is regenerated and reviewed on every candidate revision; it is never silently
updated by a transitive dependency refresh.

The profile uses BoringTun's library `Tunn` API for NoiseIK and WireGuard packet
crypto and smoltcp's explicit-buffer IP/TCP socket state machines. Their primary
documentation respectively describes BoringTun as a WireGuard client library
and smoltcp as a stack with application-owned socket buffers
([BoringTun 0.7.1](https://docs.rs/boringtun/0.7.1/boringtun/),
[smoltcp 0.14.0](https://docs.rs/smoltcp/0.14.0/smoltcp/)). The Pohunek adapter
joins them over a single ordinary UDP socket; it does not use BoringTun's
TUN-oriented CLI, a kernel network interface, system routes, `wg`, `wg-quick`,
or a privileged helper.

Before #72 can select a revision, it must record a disposition of every relevant
newer upstream report. In particular, [BoringTun #494](https://github.com/cloudflare/boringtun/issues/494)
is an open report that 0.7.1 lacks WireGuard's 16-byte data padding; it is not
an interoperability claim and must be checked by a residue-0..15 packet-length
differential golden against a known conforming peer. [BoringTun #495](https://github.com/cloudflare/boringtun/issues/495)
is an open report of one-way loss 15--30 seconds after handshake in the precise
library-`Tunn`/smoltcp/no-TUN pattern. It is relevant evidence, not a known
Pohunek defect: reproduce its sustained bidirectional differential golden on the
candidate and 0.6.0 control, including timer cadence and packet captures with
payload redaction. If either report affects the candidate, the affected revision
is a release blocker until a reviewed project-owned repair or an upstream fixed
revision is pinned with a regression golden. A passing handshake alone is never
a disposition. The WireGuard protocol's time-based rekey and keepalive behavior
is the primary timer reference ([WireGuard protocol](https://www.wireguard.com/protocol/)).

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

The inner path is IPv4 only. The proposed inner MTU is 1,280 bytes and TCP MSS
is 1,224 bytes (`1,280 - 20` IPv4 header `- 20` TCP header `- 16` worst-case
padding reserve); smoltcp must account for TCP options and reduce the offered
MSS further when required. The
WireGuard transport packet adds 32 bytes before outer IP/UDP. Thus the default
outer IPv4 datagram is at most 1,340 bytes (`1,280 + 32 + 8 + 20`) and outer
IPv6 is at most 1,360 bytes (`1,280 + 32 + 8 + 40`). For #494 evaluation,
the selected implementation must pad transport plaintext by zero through 15
bytes to a multiple of 16. That is an acceptance obligation, not a claim about
stock 0.7.1: if the report reproduces, #72 must pin a reviewed project patch or
upstream fixed revision before using these maxima. The repaired implementation
must budget padding outside the IPv4 packet but within the 1,280-byte encrypted
plaintext limit, so it does not increase either outer maximum. Its differential
golden must prove both outer-length residues and that a conforming peer recovers
the exact IP length while ignoring only authenticated trailing padding.
The adapter rejects an inner packet larger than the configured inner MTU before
crypto or smoltcp, does not emit IPv4 fragments, and advertises/reduces MSS on a
confirmed lower PMTU. A black-holed PMTU signal, unavailable ICMP, or a path
below the configured safe floor causes bounded connection failure and reconnect,
not outer fragmentation or an unbounded probe loop. Outer IPv4 and IPv6 are both
supported UDP underlays; neither changes the IPv4-only tunnel address space.

### 10.3 Resource bounds

Packet sizes, fragment behavior, handshake rates, peers, TCP sockets, socket
buffers, retransmission queues, keepalive cadence, idle deadlines, and reconnect
backoff are configurable with hard maxima. The profile admits at most 10 peers,
one host-link TCP socket and up to 10 host-initiated attach sockets per peer
(11 TCP sockets at the worst concentrated host), and no general UDP or arbitrary
TCP service. Governance/cancellation capacity is reserved on that single
authenticated host link; it cannot create a second dispatcher, subscription,
identity context, or API path. Every UDP datagram is length-checked
against the outer maximum before BoringTun; decrypted packets are length-checked
against the inner MTU before smoltcp. RX/TX UDP buffers, per-peer crypto queues,
smoltcp TCP RX/TX buffers, timer work, and retransmission queues have independent
byte and entry caps and return typed overload/drop outcomes rather than allocate.

The event loop drives BoringTun timers and smoltcp polling from one monotonic
deadline source; it schedules the earliest crypto, TCP retransmission, keepalive,
idle, or reconnect deadline, drains bounded work, then yields. Rekey, NAT source
port/address rebinding, peer restart, idle/keepalive, and reconnect-storm paths
must preserve this bound and cannot create duplicate peer/socket state. #72 must
prove it in an unprivileged two-endpoint harness with no network capabilities or
TUN device: known-answer and cross-implementation packets; truncated, oversized,
padding-residue, and output-buffer-exhaustion fuzz/property tests with zero
panics; and sustained bidirectional TCP under repeated rekeys, NAT rebinding,
idle/keepalive, loss, reorder, peer restart, PMTU reduction, and all-10-host
reconnect storm. Each run records candidate checksums/features/MSRV, timer and
buffer limits, packet counters, losses, reconnect/convergence distributions,
CPU and RSS. It is acceptance evidence to be produced by #72, not a measured
claim in this RFC.

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

### 11.3 Concurrent dispatch and terminal writers

The relay link has bounded concurrent in-flight dispatch, exactly one serialized
NDJSON writer and one subscription. Per-share queues are fair; governance,
revocation, cancellation and snapshot work reserve capacity and preempt ordinary
work. All queues/deadlines are bounded; slow writer cancels dependents and closes
the link. Responses remain ordered per operation and events per subscription,
while independent operations may interleave. Multiple terminal writers are
allowed. Input orders per attach stream and interleaves at worker arrival. Resize
uses existing source monotonic sequences, global serialization across sources,
last successfully applied geometry, and `attach_with_size` participation.

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

### 12.5 Immutable binding and operation journal

`ResourceBindingV1` is the immutable tuple `(origin, host_share_id,
share_revision_at_creation, profile_id, profile_revision, project_id,
project_revision, root_file_identity, canonical_root, launch_parameter_digest)`.
`root_file_identity` is the device/inode (or platform equivalent) obtained from
an opened root descriptor. The daemon persists this tuple in the same local
transaction that makes the session visible. Observed process/OSC-7 cwd or
detected project is projection metadata only and can never create authority.
Before every create, fork, resume, native recovery, diff, hook, worktree create
or removal, it reopens and canonicalizes source and destination beneath approved
roots, compares descriptor identity and current approved profile/project revision,
then performs the side effect from that descriptor. A deleted/changed relay
profile yields `approved_profile_unavailable`; frozen resume fallback is never
authority. Symlink replacement, escape, cross-share source, or a changed root
yields a typed denial before worker registration.

Relay mutation begins with `operation.ticket.issue`, not caller-chosen random
idempotency. The daemon atomically admits an unconsumed ticket in the current
enrollment/recovery namespace and returns a MAC-protected opaque ticket carrying
host, enrollment, generation, share/revision, method class, issue/expiry and
random nonce. A lost issue acknowledgement is retried by its relay correlation
key and returns the same ticket. A non-ticket or invalid/foreign/expired ticket
is always rejected, including after result compaction; therefore a delayed create
can never be mistaken for a new operation.

`operation.begin` carries the ticket and full bounded versioned typed payload.
The daemon, never the relay, validates and canonicalizes that encoding and then
computes the HMAC of complete payload including terminal input under its local
journal key. Raw input is never retained. The relay retains correlation-to-ticket
mapping through ticket expiry and never mints a replacement after a lost issue
acknowledgement. The durable record uses CAS states: `issued`, `begun`,
`resources_prepared`, `irreversible_commit`, `result_recorded`, `cancelled`, and
`cleanup_complete`. Same ticket and fingerprint returns its typed safe result or
`in_progress`; a changed fingerprint is `ticket_payload_mismatch`. The ticket
is retained as a rejected tombstone through its encoded expiry. The complete
minimal typed result remains through ticket expiry and relay durable creator/ACL
reconciliation acknowledgement: session ID/origin, ResourceBinding/revisions,
and safe owned worker/worktree/native references. Bounded outstanding tickets
apply backpressure before admission. Only after both conditions may evidence
compact to safe digest/status; post-expiry lookup may report reconciliation
evidence but never executes. `operation.result.get` returns the retained result.

Each enrollment/recovery namespace also persists a monotonic `ticket_expiry_floor`
separate from catalog retirement. Before deleting any expired ticket/result row,
the daemon advances that namespace floor to at least the ticket expiry and fsyncs
the floor transaction; only then may compaction delete the row. `operation.begin`
rejects any ticket with `expires_at <= ticket_expiry_floor` regardless of wall
clock, so a restart or clock rollback cannot admit a MAC-valid compacted ticket.
Read-only result lookup may return an exact still-retained result for an
outstanding relay reconciliation acknowledgement, but it never reopens execution.

Cancellation or revocation wins before `irreversible_commit`; it CASes to
`cancelled` and runs cleanup. Once the irreversible commit CAS wins, it returns
the one committed result even if cancellation arrives later. A terminal-input
disconnect after begin returns `input_outcome_unknown`; it is never replayed.
Creator/ACL intent is durable at the relay before ticket issue; no session is
visible there until host result reconciliation confirms its exact origin. Cleanup
durably records hook invocation, worktree creation, worker spawn, registration
and removal stages. Before hook start, during hook execution, and after return
before completion fsync, cancellation/revocation kill barriers are checked. A
crash in that interval becomes durable `external_effect_unknown`, quarantines the
operation and blocks team routing/new worker; it never automatically retries an
arbitrary hook. Local owner repair explicitly resolves it. Recovery cleans only
recorded resource identities; it never kills ambiguous/pre-existing resources or
claims to undo external hook effects. #70 owns ticket protocol persistence, #82
resource/lifecycle enforcement, and #71 relay intent reconciliation. Their tests
cover every CAS/stage, lost ACK, delayed ticket, changed input, barrier, hook
ambiguity, revocation race, symlink swap, transitive lifecycle, and compacted
ticket followed by clock rollback, daemon restart, and repeated same-ticket begin.

## 13. Relay identity and authorization

Issue [#85](https://github.com/zajca/pohunek/issues/85) owns the PostgreSQL
identity, credential, audit, and admission primitives. Issue
[#92](https://github.com/zajca/pohunek/issues/92) owns the reference social
broker, external-evidence verification, and self-service admission. #72 may
consume only the completed authenticated enrollment transaction; it must not
invent a parallel identity flow.

### 13.1 Human flows and reference issuer

The pinned reference deployment is Keycloak **26.6.2**, with its exact release
artifact digest, realm export without secrets, enabled broker providers, client
redirect URIs, and extension versions recorded as deployment evidence. The
reference is not a claim that later Keycloak releases are compatible. Its
official [26.6.2 release note](https://www.keycloak.org/2026/05/keycloak-2662-released)
and [upgrading guide](https://www.keycloak.org/docs/latest/upgrading/) are the
release and migration sources which #92 must review before pinning it.

Browser users use Authorization Code with PKCE through the relay's registered
HTTPS callback. The relay creates a one-use `BrowserLogin` row before redirect:
`login_id`, a secret digest of `state`, secret digest of `nonce`, PKCE verifier
digest, exact redirect URI, Keycloak issuer/client/audience, requested action,
account-link generation, recovery generation, creation and expiry timestamps,
and `unused` state. The callback atomically consumes that row before code
exchange; a mismatched, expired, replayed, or recovery-generation-stale callback
is rejected and audited. A server-side browser session is a random opaque cookie
whose digest, principal, session generation, expiry, idle deadline, and recovery
generation are stored in PostgreSQL. Cookies are Secure, HttpOnly and SameSite;
mutations require CSRF and WebSocket upgrades require an exact allowed Origin.

CLI login and host enrollment use only the configured issuer's Device
Authorization Grant. The relay creates a one-use, transaction-bound device row
with the exact requested action, audience, expiry, polling interval, nonce,
principal/account-link generation, and recovery generation. The terminal sees
only the bounded user code and verification URI returned by the broker. There
is no loopback callback, generic missing-Origin exception, or browser-cookie
substitute for CLI/enrollment. A missing device endpoint, expired/denied code,
wrong audience, or changed recovery generation fails the action; it never falls
back to a less constrained flow.

After either flow, the relay validates discovered issuer metadata and the token
signature, issuer, audience/authorized party, expiry/not-before, nonce where
present, subject, and the exact transaction binding. Humans are keyed by
`(issuer, subject)`. Email, display name, Google `hd`, GitHub login, and broker
attributes are attributes, never keys. Linking Google and GitHub identities is
an explicit authenticated, audited transaction requiring both stable identities;
matching email strings never link accounts.

### 13.2 Versioned verified external evidence

The relay accepts evidence only in one PostgreSQL transaction after a successful
authenticated broker exchange and the broker extension's authoritative provider
check. Version 1 has
these immutable fields: `evidence_version`, `evidence_id`, `principal_id`,
`issuer`, `keycloak_subject`, `provider`, `provider_subject`, `team_id`,
`admission_rule_id`, `admission_rule_revision`, `account_link_generation`,
`provider_identity_generation`, `checked_at_utc`, `checked_monotonic_epoch`,
`expires_at_utc`, `method`, `outcome`, and a keyed digest of the binding
transaction. It is audience-bound to the relay `RelayId` and target team; it
cannot be copied to another relay, team, rule, or account link. `outcome` is
`eligible`, `ineligible`, or `unknown`; only `eligible` can authorize access.
The row is append-only evidence, while the current decision is a separately
revisioned projection so denial, removal, and account-link changes can cancel
access immediately.

The broker extension serializes its attestation as canonical JSON and signs it
with an active Keycloak evidence-signing key identified by `signing_key_id`.
The relay pins issuer, audience, schema version and public-key set. Key rotation
overlaps old/new keys only through the maximum evidence lifetime, then retires
the old key. The relay rejects unknown, retired, wrong-audience, altered,
generation-stale, or replayed attestations before authorization. The PostgreSQL
row remains the authoritative revocation source; a valid signature is evidence,
not a reusable bearer grant.

The Keycloak integration has one narrow, versioned server-extension boundary:
the `pohunek-evidence-v1` Keycloak provider performs the actual provider check,
then calls the relay's authenticated internal `POST /internal/evidence/v1/issue`
endpoint over mTLS. Before the check, the relay issued a one-use challenge bound
to relay ID, audience, authenticated transaction ID, expected provider subject,
nonce, account-link/rule generations, and expiry. The signed attestation returns
that challenge, upstream subject, nonce, actual upstream `checked_at`, outcome,
method and a digest of the upstream token/code, never raw upstream credentials.
The relay validates schema/key/issuer/audience, challenge consumption, replay,
rule/account-link generations and signature atomically. Signer rotation and a
crash between challenge consumption and evidence commit are covered by a durable
challenge/result transaction: retry returns the same evidence or safely fails;
it cannot issue a second proof. The endpoint accepts no browser cookie or relay
user credential and has no general query capability. The provider artifact digest
and Keycloak SPI/API compatibility version are #92 release evidence; mismatch
fails startup rather than silently dropping checks.

`expires_at_utc` is at most 60 minutes after the actual upstream check. While
the relay authorization process remains running, it enforces both its monotonic
elapsed-time deadline and persisted wall deadline; a backwards wall clock is
fail-closed. Restart of that authorization process invalidates its cached human
evidence because its monotonic clock disappeared, so a fresh upstream check is
required before human access resumes. A broker-only restart or outage while the
relay remains running does not extend or reissue the prior relay-verified proof:
it remains usable only until its original deadline. Any newly issued evidence
still requires a new challenge and fresh upstream check. Cache reads, broker
profile attributes, locally refreshed relay or Keycloak tokens, user activity,
failed requests, retries, and restart never advance `checked_at_utc`. An unknown
result, rate limit, broker outage, provider outage, or expired proof removes
authority at the prior deadline and cancels that principal's control, event,
observation, and attach streams. It does not stop host sessions or affect other
valid participants.

For a Google Workspace rule, the broker extension bypasses cached Keycloak SSO
and starts a new upstream Google authorization-code transaction with new state
and nonce. It accepts a one-use returned code only after a new upstream exchange
and validates the signed Google ID token `iss`, `aud`, immutable `sub`,
`email_verified`, exact `hd`, `nonce`, `iat`, and `exp`; `checked_at` is that
upstream exchange time, not receipt of a cached token. An email suffix or
`login_hint` is insufficient. Google
documents that `sub` is the stable key and that `hd`, rather than email domain,
identifies a Workspace/Cloud organization in its [OIDC reference](https://developers.google.com/identity/openid-connect/openid-connect).
The user may need a new account-selection and consent interaction; consent alone
is not reauthentication proof. The extension does not assert `prompt=login` or
`max_age` support and does not treat optional `auth_time` as proof. Inability to
obtain the fresh response is `unknown`.

For a GitHub organization rule, the broker extension uses its protected upstream
authorization, never relay PostgreSQL, with `Accept: application/vnd.github+json`
and `X-GitHub-Api-Version: 2022-11-28`. It resolves the configured target
organization to its stable numeric ID, first calls `GET /user` and binds numeric
user `id`, then calls `GET /user/memberships/orgs/{org}` and binds that returned
organization numeric ID. Only `state: active` is eligible; `pending` is
ineligible. A 404 is authoritative absence only when the pinned API call proves
the broker token's visibility, scope, and organization access; otherwise 403,
404, rate limit, subject/org mismatch, and unavailable are `unknown`. A forbidden
or ambiguous missing response never proves removal. The broker authorization requires
`read:org` for the supported OAuth flow (or documented equivalent Members-read
permission); private membership and SAML/OAuth restrictions fail closed if no
authoritative answer is possible.
The [GitHub membership endpoint documentation](https://docs.github.com/en/rest/orgs/members)
defines that endpoint, permission, and state. Webhooks are observations with
source-delivery gaps; they may trigger an early recheck but never renew evidence.
#92 proves real removal on a dedicated test organization by a new authoritative
recheck yielding confirmed `ineligible` and immediate stream cancellation; HTTP
error status alone is never used as false confirmation.

### 13.3 Admission and active authorization

A matching fresh evidence row permits a configured self-service rule to create
only a `Member` membership. The admission transaction checks current team
capacity/quota, current rule revision, local administrator deny generation, and
recovery generation while writing membership, decision audit, and cancellation
registration atomically. It grants no Owner/Admin role, no share, profile,
project, session ACL, or capacity by itself. A local administrator deny survives
provider relinking and automatic rechecks until an explicit audited reversal.
Teamless authentication may create a principal and a relay-native credential but
authorizes no team resource; it remains valid for its exact device-bound host
enrollment transaction only when the enrollment policy allows the principal.

On successful login the relay issues a random human credential once for CLI
storage in the OS keyring; it stores only a keyed digest, principal, credential
generation, expiry, revocation state, last-used metadata, and recovery
generation. Browser cookies are not bearer credentials, and browser sessions
cannot authenticate the CLI. Every sensitive decision and active stream checks
current credential, membership, grant, policy/recovery generation, evidence
deadline, share revision, and cancellation registry. Credential rotation,
revocation, local deny, membership removal, rule change, evidence expiry, and
restore quarantine cancel affected idle and active streams promptly.

### 13.4 Service accounts and human credentials

A service account is a first-class principal. It authenticates with a random
high-entropy credential consisting of a public credential ID and secret. The
relay stores only a keyed digest, state, timestamps, and audit metadata.
Credentials have mandatory expiry, rotation overlap, last-used tracking, and
immediate revocation. Raw credentials are shown once, never accepted in URLs or
argv, and never logged.

Credentials carry no embedded authorization scope. Current team membership,
roles, grants, team-disable state, recovery generation, credential expiry and
revocation are resolved server-side on every authorization decision so revocation
does not wait for credential expiry. The 60-minute external-evidence requirement
applies only to human access governed by an external-eligibility rule; service
accounts never satisfy or bypass that human self-service rule.

### 13.5 Teams, groups, and roles

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

### 13.6 Ownership and test summary

The evidence, admission, credential, cancellation, durable state, and
adversarial test contract is defined by §§13.1–13.5. #85 owns database/auth
primitives and transaction tests; #92 owns the Keycloak extension, Google/GitHub
freshness, signer rotation, challenge crash/replay, local-deny and join tests;
#71 and #86 consume the current-decision check on every public action and stream.
#92 also tests broker restart/outage with an old attestation: the relay may use
only its unchanged original deadline while still alive, cannot reissue evidence
without a fresh challenge/check, and cancels access at expiry. #85 tests relay
authorization-process restart invalidating cached human evidence while preserving
ordinary credentials and memberships.

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
and team web workspace. Team browser clients never send arbitrary daemon NDJSON
through a transparent tunnel. The existing owner browser client continues to
use the transparent `web/backend` transport because browsers cannot dial the
owner Unix socket or direct daemon TCP listener themselves.

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

One daemon projection coordinator owns the commit order for safe session fields,
notification state and active-share projection. It writes the changed host state
and next sequence in one critical section before publication; no producer sends
an event directly. `subscribe.freeze` first registers the sole subscriber at
sequence `S`, then the coordinator freezes all three projections and allocates
watermark `W >= S`. Events committed after W enter that subscriber's bounded
queue. The frozen manifest contains snapshot ID, daemon epoch, W, total bytes,
items and parts, and SHA-256 of each indexed part and of the canonical manifest.

1. The relay requests subscribe/freeze and receives the manifest plus parts.
2. Each part is independently frame-limited and authenticated by manifest index
   and hash; no part can be substituted across epoch/snapshot.
3. The relay validates the complete manifest and parts in memory/spool bounds.
4. One PostgreSQL transaction installs manifest, all projection rows and W as
   current, discards buffered events at or below W, and records next expected W+1.
5. It applies higher events in order, each with a conditional expected-sequence
   update. Duplicate already-committed events are ignored only for same epoch.
6. Queue overflow, sequence gap, epoch/generation/link change, invalid part,
   transaction failure, cancellation, or timeout rolls back/notifies no current
   install, marks the host degraded, drops cache, and repeats the full procedure.

The relay never continues from a suspected gap and never asks the daemon for
replay. PostgreSQL may retain the last catalog as explicitly stale UI data, but
it is not used for authorization or mutation routing until a current snapshot
is installed.

### 16.3 Frame and queue bounds

Each multipart frame is below the existing 1 MiB protocol limit. The proposed
limits profile supplies `max_snapshot_parts`, `max_snapshot_total_bytes`,
`max_snapshot_items`, `max_snapshot_duration`, `max_event_entries`,
`max_event_bytes`, and `max_projection_spool_bytes`; all are enforced separately.
Event queues satisfy both entry and byte formulas: admitted maximum event rate
times maximum snapshot duration plus reserved cancellation/revocation margin.
Revocation is coordinator-priority work; it commits a sequence and cancellation
before ordinary snapshot publication. #84 owns deterministic concurrent mutation,
notification/share change, malformed manifest, missing part, database rollback,
overflow, duplicate/gap, revocation-during-freeze and reconnect tests.

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

### 17.3 Catalog retirement and deletion

Catalog retention is distinct from ACL, audit, ticket tombstones, admission,
evidence and revocation safety records. Terminal lifecycle stores immutable
`ended_at`, `catalog_retire_at` and terminal reason. The default is 30 days;
team configuration may reduce or increase it only within operator hard bounds.
Manual deletion immediately creates a durable suppression record and removes
the relay catalog projection. It never removes a host session, PTY, worker or
worktree, and it does not shorten audit or safety retention. Audit defaults to
90 days and is operator-controlled.

Each host has a monotonic `retirement_sequence`, a durable host-side retirement
checkpoint and relay-side acknowledged contiguous floor. Suppression records
carry that sequence; host snapshot/reconnect acknowledges the greatest contiguous
sequence it has durably applied. Holes remain records, bounded by the configured
hole limit; only records at or below the acknowledged contiguous floor and past
the old-host reconnect horizon may compact. The relay persists a non-decreasing
compacted floor independently from ordinary catalog backup. A reconnecting host
whose checkpoint is behind that floor is fenced from catalog publication, receives
the retained suppression/checkpoint proof, durably advances, then may snapshot.
An old host backup therefore cannot resurrect a deleted catalog entry. Quota-full
is a typed admission failure before new catalog/session reservation and preserves
governance reserve. #84/#87 own deterministic delete-before-end, hole, old-backup,
ack-loss, compaction, reconnect and quota-full tests.

The host recovery manifest includes `retirement_sequence`, durable retirement
checkpoint, acknowledged contiguous floor and compacted-floor witness. Restore
and host-link reconciliation reject a manifest below the independently retained
floor until local repair advances it; #87 tests witness rollback and #84 tests
manifest/floor mismatch with snapshot recovery.

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
overload error. No unbounded channel is permitted. The fairness and owner-reserve
claim below applies only to configured relay allocations and reserved
control-plane capacity under the supported load profile. It does not claim
hostile same-UID CPU, RAM, disk, or process isolation; [#88](https://github.com/zajca/pohunek/issues/88)
owns that boundary. Local Unix and direct-NetBird owner operation do not depend
on relay admission, but ordinary OS resource exhaustion remains outside this
relay scheduler claim.

### 18.1 Proposed unmeasured reference profile

All values in this section are **PROPOSED UNMEASURED** design-acceptance
candidates. They are inputs to #72/#84/#87 implementation and measurement, not
shipped configuration defaults, product maxima, or benchmark results.

The profile has one relay plus PostgreSQL and 10 Linux hosts. Each is a
four-vCPU, 8 GiB RAM, NVMe-backed x86_64 Linux machine; the relay and PostgreSQL
may be separate machines of that specification, each with a 1 Gbit/s network
interface. The harness shapes every host-to-relay path to 20 ms RTT, 1%
independent packet loss, 0.1% reorder, and 20 Mbit/s in each direction. It holds
50 total live sessions, 10 simultaneous
interactive attach streams. Its multiwriter case has 10 streams arranged as five
sessions with two authorized writers each (and separately exercises 10 writers
on one session); it never calls 20 streams "10 interactive streams". It retains
500 ended catalog entries per host (5,000 total, budgeted at 1 KiB indexed metadata each) and
2,000 notifications per host (20,000 total, budgeted at 512 bytes each): this
is intentionally substantially larger than one protocol frame. History is
paginated; a current snapshot is not an offline terminal-history substitute.

| Resource or rate | Proposed unmeasured allocation | Accounting and overload behavior |
|---|---:|---|
| Public relay requests | Relay-wide: 250/s sustained; 500/s for 30 s burst | Body and response each <= 512 KiB; excess receives typed overload before host dispatch. |
| Admitted host projection events | Per host: 500/s sustained; 1,000/s burst. All-10-host storm: 5,000/s sustained; 10,000/s burst. | Each entry <= 1 KiB. A host queue holds 12,288 entries / 12 MiB: `1,000 × 10 s + 16` revocation entries = 10,016 KiB required, leaving 2,272 entries / about 2.2 MiB headroom. |
| Snapshots | Per host snapshot: <= 4 MiB, <= 16 parts, <= 10 s. All-10-host storm: <= 40 MiB aggregate and 10 concurrent snapshots. | Each part <= 256 KiB, within the existing 1 MiB frame ceiling; manifest, parts, and projection spool have separate bounds. A 4 MiB host snapshot in 10 s consumes 3.36 Mbit/s. A stalled snapshot releases ordinary slots but keeps the reserved cancellation path. |
| Dispatch fairness | 32 operations / 256 KiB per share; 64 concurrent ordinary host RPCs relay-wide | Round-robin across nonempty shares; exceeding either per-share dimension stalls/overloads that producer only. |
| Governance/cancellation reserve | 16 operations / 128 KiB relay-wide, separate from ordinary dispatch | Revocation and cancellation use this reserve before ordinary work; it is not workload resource isolation or the host owner reserve. |
| Host owner-control reserve | 16 operations / 128 KiB per host daemon control queue | Local Unix/direct-NetBird owner `inspect`/`stop`/`wait` bypass relay scheduling and consume this separately reserved host control capacity. |
| Interactive streams | 10 total; multiwriter uses 10 streams as five two-writer sessions, or 10 writers on one session | 64 KiB relay attach buffer per direction/stream, 1.5 MiB aggregate per host for the worst 10-stream concentration, and 3 Mbit/s aggregate relay attach bandwidth across all 10 streams; slow producer is closed without retaining terminal bytes. |
| Transport sockets and buffers | <= 10 peers; <= 11 TCP sockets per peer; 256 KiB UDP RX + 256 KiB UDP TX per process; 128 KiB TCP RX + 128 KiB TCP TX per socket | The 11 sockets are one host link and up to 10 attach streams. Governance/cancellation reserve is queue capacity on the sole host link. Socket, crypto, TCP, and application queues are independently counted; no buffer borrows either reserve. |
| PostgreSQL/admission | <= 128 concurrent relay DB operations; <= 32 cancellation/audit operations reserved | A failed durable authorization/audit transaction denies new sensitive work; revoke/deny continues through its reserved path. |

At the stated maximum, the event queue arithmetic is deliberately based on the
burst, not average, rate: `1,000 entries/s × 10 s + 16 reserved = 10,016`
entries and `10,016 × 1 KiB = 9.79 MiB`; the proposed 12,288-entry / 12 MiB
bound covers both dimensions without assuming average-size events. During the
10-second snapshot window, one host's 1,000 1-KiB events/s budget consumes 8.19
Mbit/s, its 4 MiB snapshot consumes 3.36 Mbit/s, and its capped attaches consume
3 Mbit/s: 14.55 Mbit/s before protocol overhead, below its shaped 20 Mbit/s
path. The all-10-host storm is therefore 81.9 Mbit/s events + 33.6 Mbit/s
snapshots + 3 Mbit/s attaches = 118.5 Mbit/s before protocol overhead; #84/#87
must measure relay ingress, CPU, database batching/latency, queue high-water
marks, and overload behavior at this 10,000-event/s aggregate, not infer it from
a one-host run. The retained catalog and notifications budget 15 MiB of indexed
payload before database-row and index overhead; #87 must publish actual storage
separately.

### 18.2 Target outcomes and measurement procedure

The acceptance targets are: relay process RSS <= 1.5 GiB and CPU <= 4 vCPUs;
each host tunnel/host-link process RSS <= 256 MiB and CPU <= one vCPU; p95/p99
owner `inspect` <= 250/500 ms, `stop` admission <= 500 ms/1 s, and `wait`
registration <= 250/500 ms. At 20 ms RTT, p95/p99 authorization-to-first-byte
targets are <= 250/500 ms; interactive input-to-host-write is <= 150/300 ms;
p95/p99 reconnect is <= 10/15 s; and an all-host current projection converges
within <= 30/45 s after relay recovery. Lease behavior remains §19's 5-second
lease with renewal at most every second; the profile does not loosen those
deadlines. These values are targets, never present-tense performance claims.

#72 measures the unprivileged transport limits in §10.3; #84 measures frozen
snapshot/event convergence; #87 runs the end-to-end profile. Each result must
state release-mode build identity, exact dependency checksums/features/MSRV,
hardware, kernel, PostgreSQL settings, network-shaper command/configuration,
configured limits, seed, and raw latency/resource distributions. Run a 5-minute
warm-up followed by a 30-minute measured interval, three cold-start and three
warm-start repetitions, then an all-10-host reconnect storm while 50 sessions
and 10 multiwriter streams remain active. Include normal load, one saturated
share, queue overflow, slow writer, database/audit denial, loss/reorder, NAT
rebinding, rekey, peer restart, PMTU reduction, revocation, and owner
inspect/stop/wait during every storm. Report p50/p95/p99, maximum, rejected and
closed counts, queue high-water marks, socket/packet drops, snapshot retries,
lease loss behavior, CPU/RSS, and database latency; retain payload-free traces
and configuration evidence. A missed target blocks the dependent acceptance or
requires an explicitly reviewed new proposed profile; it cannot be hidden by
raising a runtime limit.

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

### 19.1 Restore quarantine and lease fencing

Recovery state is not stored solely in the restorable PostgreSQL database. The
deployment keeps a separately protected, append-only local witness containing
`RelayId`, recovery generation, witness sequence, manifest digest, key ID,
signature, catalog-suppression floor, and active-run latch. Its key and durable
storage are backed up/restored by a procedure
independent of the PostgreSQL backup. If the witness cannot be read, verified,
advanced, or durably written, the relay fails stopped: it opens no public ingress,
host link, or sensitive management route. This detects a restore to an older
known witness; it does not claim detection of matching old database and witness
copies without an independent witness.

The supported restore phases are: (1) stop ingress and let the current lease
expire; (2) advance and durably verify the witness generation before restoring
the database; (3) start in `recovery_quarantine`, invalidate every browser
session, human/service credential, device/browser/enrollment/attach transaction,
authorization decision, cancellation lease, and host-link binding from an older
generation; (4) produce an operator review manifest of principals, account links,
memberships, rules, local denies, grants, ACLs, shares, catalog retention state,
catalog-suppression floors/checkpoints, and outstanding incidents; (5) reconcile
current host snapshots and require
fresh login and external evidence; (6) commit one audited `reopen` decision
naming the reviewed manifest digest and new witness generation; only then open
team ingress. Keycloak broker sessions, upstream tokens, and account-link state
never bypass quarantine. Broker restore requires the same review and explicit
reopening. Ordinary process restart retains the unchanged generation and valid
state; it is not a restore and must not invalidate credentials merely because
the process restarted. External eligibility is different: after relay
authorization-process restart it is `unknown` until a fresh upstream check
completes. A broker-only restart/outage does not extend the original evidence
deadline while the relay process remains alive. Within a running verifier,
monotonic elapsed time and the wall deadline are both enforced; a backwards wall
clock is fail-closed. A local fsynced time floor may detect regression but is not
treated as trusted elapsed time across a reboot.

The single active process holds a fenced PostgreSQL lease with a **5-second**
expiry, renewed at most every **1 second**. The row contains `RelayId`, process
instance ID, random fence token, recovery generation, expiry, and monotonic
heartbeat sequence. Every public authorization, host dispatch, event delivery,
attach byte forwarding, and idle-stream timer validates the current fence token,
generation, and lease deadline before forwarding; idle streams are rechecked at
least every **100 ms**. On failed renewal, database disconnection, stale fence,
or deadline expiry, the process immediately stops ingress, cancels streams,
closes host forwarding, and refuses new work. A later advisory lock acquisition
does not make an old paused process safe. Tests pause/resume a process, sever its
database connection, retain idle sockets past expiry, and race two instances;
only the current fence may forward bytes or decisions.

Before normal ingress, the process fsyncs an `active-run` latch in witness
storage. An unclean prior latch forces startup review; only orderly shutdown may
clear it after ingress and forwarding are closed. Sensitive admission writes its
required decision audit transactionally before granting access. If
PostgreSQL/audit is unavailable, no new sensitive grant may be made; the relay
must not pretend it recorded an audit row. Revocation and deny immediately
cancel affected streams while a durable fail-closed marker is written before the
revocation acknowledgement completes. That marker has only safe incident and
scope coordinates and prevents an old grant from reviving after restart. Failure
to persist it, failure of any witness storage, or an unclean active-run latch
leaves the latch dirty, fails the process stopped, and requires startup review;
the design never pretends an fsync succeeded on a failed disk. #85 owns witness,
lease, latch, audit/admission transactions, and crash tests; #87 owns rehearsal,
retention, catalog-suppression-floor recovery, and operations.

## 20. Deployment and operations

The supported first deployment is one active unprivileged `pohunek-relayd`
process, PostgreSQL, one public HTTPS endpoint, and one public WireGuard UDP
endpoint. It can run under systemd or in a container without `CAP_NET_ADMIN`.

Exactly one active process may own a `RelayId`; the fenced lease in §19.1,
rather than advisory locking alone, prevents concurrent forwarding. Backup
includes PostgreSQL and separately protected relay secret/key/witness material.
The restore runbook advances and verifies the independent witness generation,
enters quarantine, and requires manifest review and one reopen commit. It does
not claim automatic detection of arbitrary complete deployment rollback without
an independent witness.

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

| Order | Issue | Status | Outcome |
|---:|---|---|---|
| Complete | [#69](https://github.com/zajca/pohunek/issues/69) | Completed | Direct-overlay foundation. |
| Complete | [#80](https://github.com/zajca/pohunek/issues/80) | Completed | Original accepted RFC/canonical documentation landing. |
| Complete | [#81](https://github.com/zajca/pohunek/issues/81) | Completed | Stable host identity, single enrollment, exact owner and local transfer. |
| Reconcile | [#91](https://github.com/zajca/pohunek/issues/91) | Pending merge | Documentation reconciliation; it is not closed by this RFC edit. |
| 1 | [#85](https://github.com/zajca/pohunek/issues/85) | Planned | Relay/PostgreSQL foundation, OIDC, durable audit/admission, recovery generation/quarantine, lease fencing, principals and credentials. |
| 2 | [#92](https://github.com/zajca/pohunek/issues/92) | Planned | Keycloak-brokered Google/GitHub admission and bounded external-evidence revalidation. |
| 3 | [#72](https://github.com/zajca/pohunek/issues/72) | Planned | Embedded userspace WireGuard, address allocation, OIDC host enrollment, and host-initiated link transport. |
| 4 | [#70](https://github.com/zajca/pohunek/issues/70) | Planned | Protocol v4 host link, daemon relay context, immutable origin, and share guard framework. |
| 5 | [#82](https://github.com/zajca/pohunek/issues/82) | Planned | Locally approved `HostShare` lifecycle and profile/project/operation/resource policy. |
| 6 | [#83](https://github.com/zajca/pohunek/issues/83) | Planned | Relay-side session visibility, creator rights, ACLs, admin authority, and revocation. |
| 7 | [#84](https://github.com/zajca/pohunek/issues/84) | Planned | Subscription-first atomic snapshot/watermark and gap-triggered resync without replay. |
| 8 | [#71](https://github.com/zajca/pohunek/issues/71) | Planned | Relay host-link manager, router, state aggregator, attach proxy, and typed public API. |
| 9 | [#86](https://github.com/zajca/pohunek/issues/86) | Planned | Team CLI and Svelte relay surface, with explicit separation from retained owner WebUI. |
| 10 | [#87](https://github.com/zajca/pohunek/issues/87) | Planned | Operational completion: audit/quotas retention, deployment, backup/restore, observability and incident evidence; not first audit persistence. |
| Later | [#73](https://github.com/zajca/pohunek/issues/73) | Post-release | Provider webhook delivery and encrypted token vault. |
| Later | [#88](https://github.com/zajca/pohunek/issues/88) | Post-release | Real profile-backed container/VM runtime isolation. |

[#69](https://github.com/zajca/pohunek/issues/69) is complete and preserves the
generic direct-overlay architecture used by NetBird. It is a prerequisite fact,
not a reason to force the relay tunnel through the direct-overlay discovery API.

The intended dependency graph is:

```text
#80 + #91 -> #85
#85 + #91 -> #92
#69 + #81 + #85 + #92 -> #72
#72 + #81 -> #70
#70 + #85 -> #82, #83
#70 + #82 -> #84
#70 + #72 + #82 + #83 + #84 + #85 -> #71
#71 -> #86
#71 + #72 + #85 -> #87
#71 + #84 + #85 + #86 + #87 -> #73
#82 + #86 + #87 -> #88
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
- under the declared supported profile, quotas and backpressure prevent one
  tenant or host from exhausting other allocations, governance reserve, or host
  owner-control reserve; this is not hostile OS/resource isolation;
- CLI and web provide the same typed permissions and failure behavior;
- the team path has exactly one auth/routing authority: the Rust relay;
- the retained owner WebUI still reaches local and direct-overlay daemons when
  no relay exists or the relay is unavailable;
- owner and team browser modes use explicit origins and adapters, never exchange
  credentials or session state, and never silently fall back between modes;
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

### 25.8 Use the Bun backend as team relay authority

Rejected. Authentication, WireGuard, routing, aggregation, audit, and quotas
must have one implementation authority: the Rust relay. This does not remove
the Bun backend's distinct production role as the owner-private local/NetBird
browser gateway.

### 25.9 Give `pohunek-relayd` a local mode

Rejected. A relay local mode would mix the public multi-tenant trust boundary
with owner Unix-socket and direct-overlay access, requiring bypasses or parallel
rules for OIDC, PostgreSQL, team authorization, host discovery, and session
visibility. The existing Bun gateway already solves browser access inside the
owner trust domain without expanding `pohunekd` or `pohunek-relayd`.

### 25.10 SQLite relay storage

Rejected. The public multi-team service needs transactional concurrency,
operational backups, migrations, and future active-passive options. PostgreSQL
is required from the first release.

### 25.11 Persist terminal output for reconnect

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
- [Keycloak 26.6.2 release](https://www.keycloak.org/2026/05/keycloak-2662-released)
  is the pinned reference broker release; #92 records the exact artifact digest,
  realm/broker configuration revision, and extension API version before use.
- [Google's OpenID Connect reference](https://developers.google.com/identity/openid-connect/openid-connect)
  defines the ID-token `sub`, `hd`, `nonce`, and optional `auth_time` semantics
  used by §13.2. [GitHub's organization-membership API](https://docs.github.com/en/rest/orgs/members)
  defines `GET /orgs/{org}/memberships/{username}`, its membership state, and
  required organization-members read permission. These are identity references;
  they do not alter the transport references above.
