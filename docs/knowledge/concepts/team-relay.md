---
type: Concept
id: concept/team-relay
title: Optional team relay
description: Implemented relay foundation and deferred architecture for sharing explicitly approved host capacity through a trusted multi-team relay.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Optional Team Relay

Status: the reduced relay foundation is implemented. It has PostgreSQL-backed
lease fencing and recovery, protected stopped-lifecycle initial Owner and
service-account provisioning, generic OIDC browser and device login, bounded
HTTPS account and credential lifecycle, and a native HTTPS/keyring CLI. It has
no host link, team client or WebUI, routing, attach path, host enrollment,
account linking, or complete team-administration API. Current Pohunek releases
otherwise use public protocol v3 through an owner-only Unix socket or a direct
configured overlay such as NetBird. The shipped Bun web backend is a transparent
mesh-local browser transport, not the team relay described here.

## Local foundation provisioning

`pohunek-relayd migrate` and `pohunek-relayd bootstrap` are protected local
procedures. Bootstrap binds exactly one configured issuer and owner-private
stable OIDC subject to an active infrastructure principal. It grants no team,
session, or implicit administrative permission.

While the relay is stopped, `pohunek-relayd provision` requires the same
owner-private identity file, an explicit team name, service-account name,
RFC 3339 expiry, and a new absolute `--credential-output` path. Its parent
directory must be owner-owned mode 0700; the artifact is created once as a
regular owner-owned mode 0600 file with no symlink following, file fsync, and
parent-directory fsync. The artifact contains the sole service credential and
is never printed, logged, returned in generic JSON output, or redelivered.

The procedure explicitly assigns the bootstrap infrastructure principal as the
new team's Owner, then creates one team-scoped service account with no grants.
It creates an expiring service credential and records audit and independently
signed witness coordinates. The command's normal result contains only durable
IDs and expiry. A collision, unsafe artifact, owner mismatch, active serving
lease, stale recovery state, or failed audit stops the operation.

An artifact written before a witness binding is an unbound orphan: it must not
be adopted or deleted by a retry. The operator must resolve that collision
manually after verifying it is safe. Once the witness binds the artifact hash,
an exact retry may resume the stopped lifecycle and verify the same durable
rows; it never creates a second credential or emits another secret.

The shipped #81 host-local foundation is deliberately narrower. It persists a
stable opaque host identity, one exact principal-or-team owner, at most one
local enrollment record, checked revisions, quarantine state, and local
transfer coordinates. Protocol v3 exposes only safe read-only inspection of
that state. It does not connect to a relay, run OIDC, create a WireGuard key,
publish a relay API, or provide a team UI.

The owner WebUI remains supported alongside the relay. Its Bun backend discovers
the local daemon and direct-overlay peers and transparently bridges browser
WebSockets into the existing owner protocol. `pohunek-relayd` has no local mode;
its future team WebUI will use a separate typed API, credential set, state
adapter, and origin. Presentation components may be shared, but there is no
cross-mode fallback or session aggregation.

The accepted design adds one optional public Rust `pohunek-relayd` authority.
Standalone and direct NetBird operation remain first-class and never depend on
the relay. A host may use either owner mode, enroll with the relay, or use both
at the same time after enrollment is implemented. NetBird is neither replaced
nor required by the relay.

## Foundation authentication and native credentials

The running relay serves a bounded HTTPS API for generic OpenID Connect browser
Authorization Code with PKCE and device authorization. Browser and bearer
authentication remain separate. Login transaction state is bound to its issuer,
redirect URI, nonce, verifier, cookie binding, expiry, and recovery generation;
browser and device transactions share one bounded pending-login capacity.

The API exposes only account, human credential, and service-account credential
lifecycle needed by this foundation. `pohunek relay login`, `status`, `logout`,
`rotate`, and `resume-rotation` use an HTTPS-only native client and origin-bound
OS-keyring storage. Credentials expire, are delivered once, and are never put in
URLs, routine output, or logs. A keyring write failure attempts compensating
revocation.

This generic OIDC implementation contains no Keycloak-specific provider or
social-account verification. Account linking is deferred to
[#107](https://github.com/zajca/pohunek/issues/107), complete team
administration to [#108](https://github.com/zajca/pohunek/issues/108), and
Keycloak-brokered Google/GitHub verification to
[#92](https://github.com/zajca/pohunek/issues/92).

## Topology and connection direction

After the host-link work lands, `pohunekd` will embed the host connector. It will
initiate a userspace WireGuard tunnel
to the relay's public UDP endpoint and then initiates every control and attach
TCP stream inside that tunnel. The relay never dials a host, and neither side
needs a kernel WireGuard interface or `CAP_NET_ADMIN`. One host can have at most
one active relay enrollment. Human CLI authorization and host enrollment use
OIDC device flow; browser login uses Authorization Code with PKCE. There is no
loopback login fallback.

The planned transport has a pinned BoringTun/smoltcp evaluation profile.
Padding, connectivity, MTU, rekey, NAT, idle, buffer, timer, privilege, and
exact-version claims remain evidence to verify, not shipped capabilities or test
results. RFC §10 assigns the contract and #72 owns implementation. Protocol v4
will reuse typed NDJSON operations and separate raw attach streams; there is no
v3 compatibility shim for that relay path.

## Ownership and sharing

A host has exactly one registered owner: a principal or a team. The shipped
host-local record already stores that opaque owner without resolving relay users
or memberships. The later relay lifecycle will let a same-UID local host
operator confirm an exact, short-lived transfer proposal; the daemon will
durably change the owner, sign the outcome, and suspend every share before the
relay conditionally updates its projection. An enrolled host may later be
shared with multiple teams through independent `HostShare` records.
The relay is multi-tenant and scopes every lookup, grant, cache entry, and audit
record to a team before authorization.

A team administrator may request a share, but the request grants nothing until
the same-UID local host operator approves it on behalf of the registered owner.
Each revisioned, default-deny share can
limit operations, registered projects and canonical worktree roots,
owner-authored agent profiles, concurrent sessions, terminal access, resource
usage, and future container or VM execution backends. Reversible suspension or
terminal revocation removes relay access immediately without stopping existing
sessions. A revoked `HostShareId` is never reused; a later approval gets a new
ID and cannot republish sessions carrying the revoked origin.

The first relay release trusts collaborators at the host Unix-account boundary.
Relay API ACLs restrict relay actions but do not isolate commands or hostile
workloads running under the daemon owner. #88 owns profile-backed container and
VM isolation after release. RFC §12 defines immutable approved resources,
revalidation, and session origin; this concept does not define a second resource
or authorization contract.

The daemon authenticates the enrolled relay and enforces the local share and
session-origin ceiling. It does not know or authorize end users, service
accounts, groups, roles, or session ACLs. Those belong exclusively to the
relay. Principal identity may reach the daemon only as bounded attribution
metadata and is never an authorization input.

## Session boundary

Session origin is immutable. Local and direct-overlay sessions are never
relay-visible or relay-controllable and cannot later be published into a
share. Only sessions created through an active `HostShare` can use the relay
path. The host owner can still manage relay-created sessions through an owner
path.

Every active team member can see only that a relay-created team session exists.
Metadata, terminal observation, input, lifecycle operations, sharing, and
removal require relay-side permission. The creator and built-in team Owner and
Admin roles receive expanded authority. Custom roles and grants can apply to
human principals, service accounts, and groups. Service credentials are
expiring and rotatable; they contain no authorization scope, and the relay
stores only a keyed digest.

## Trust, persistence, and recovery

The relay process and its operator are trusted for transient terminal plaintext
and the full authority of all active shares. A compromised relay can exercise
every permission those shares allow. Application RBAC prevents an ordinary
infrastructure-administrator account from reading team sessions, but it cannot
protect against an operator with process access. Direct-host profiles also run
under the daemon owner's account and are not hostile-workload isolation;
container and VM isolation remains separate future work.

The implemented relay foundation stores its identity, authentication,
credentials, recovery state, and structured audit metadata in PostgreSQL. It
fails stopped on fencing, database, witness, audit, or recovery failure. #87
supplies operational retention and load evidence. PTY output,
input, prompts, terminal snapshots, file contents, and raw secrets are never
persisted by the relay or included in its logs or audit records. RFC §19 defines
recovery generation, while §17 defines catalog retention, audit, and admission.

Recovery uses one host-scoped subscription and the relay must discard its live
cache after a gap, overflow, epoch change, malformed event, or reconnect. The
ordered-writer, bounded in-flight, cancellation, fairness, snapshot installation,
and convergence rules are in RFC §§11 and 16. A snapshot is not offline history.
Relay outages do not stop host sessions or affect local and NetBird owner
operation; revoked-share sessions continue permanently as owner-only.

## Implementation references

The complete normative contract is in
`docs/design/team-relay-control-plane-rfc.md`, especially its identity,
recovery, scheduling, resource, snapshot, catalog, audit, and dependency
sections; use the
[source map](../assistant/source-map.md) to locate it in a source checkout. The
umbrella is [#56](https://github.com/zajca/pohunek/issues/56); implementation is
split among completed host identity [#81](https://github.com/zajca/pohunek/issues/81),
the reduced relay foundation [#85](https://github.com/zajca/pohunek/issues/85),
account linking [#107](https://github.com/zajca/pohunek/issues/107), team
administration [#108](https://github.com/zajca/pohunek/issues/108), verified
Keycloak-brokered external evidence [#92](https://github.com/zajca/pohunek/issues/92),
then transport [#72](https://github.com/zajca/pohunek/issues/72), protocol v4
[#70](https://github.com/zajca/pohunek/issues/70), shares
[#82](https://github.com/zajca/pohunek/issues/82), session authorization
[#83](https://github.com/zajca/pohunek/issues/83), synchronization
[#84](https://github.com/zajca/pohunek/issues/84), relay routing
[#71](https://github.com/zajca/pohunek/issues/71), clients
[#86](https://github.com/zajca/pohunek/issues/86), operations
[#87](https://github.com/zajca/pohunek/issues/87), then provider delivery
[#73](https://github.com/zajca/pohunek/issues/73) and workload isolation
[#88](https://github.com/zajca/pohunek/issues/88). Until those issues land,
`docs/public-api.md` is authoritative for shipped v3 behavior.
