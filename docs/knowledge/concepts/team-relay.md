---
type: Concept
id: concept/team-relay
title: Optional team relay
description: Accepted, not-yet-implemented architecture for sharing explicitly approved host capacity through a trusted multi-team relay.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Optional Team Relay

Status: accepted design, not implemented. Current Pohunek releases use public
protocol v3 through an owner-only Unix socket or a direct configured overlay
such as NetBird. The shipped Bun web backend is a transparent mesh-local
browser transport, not the team relay described here. Do not suggest relay
commands or configuration until their owning issues have shipped.

The accepted design adds one optional public Rust `pohunek-relayd` authority.
Standalone and direct NetBird operation remain first-class and never depend on
the relay. A host may use either owner mode, enroll with the relay, or use both
at the same time. NetBird is neither replaced nor required by the relay.

## Topology and connection direction

`pohunekd` embeds the host connector. It initiates a userspace WireGuard tunnel
to the relay's public UDP endpoint and then initiates every control and attach
TCP stream inside that tunnel. The relay never dials a host, and neither side
needs a kernel WireGuard interface or `CAP_NET_ADMIN`. One host can have at most
one active relay enrollment.

The planned transport uses pinned released BoringTun and smoltcp dependencies.
Protocol v4 will reuse Pohunek's typed NDJSON daemon operations and separate raw
attach streams across the host-initiated link. There is no planned v3
compatibility shim for that relay path.

## Ownership and sharing

A host has exactly one registered owner: a principal or a team. `pohunekd`
stores that opaque owner record authoritatively without resolving relay users or
memberships. A same-UID local host operator confirms an exact, short-lived
transfer proposal; the daemon durably changes the owner, signs the outcome, and
suspends every share before the relay conditionally updates its projection. An
enrolled host may be shared with multiple teams through independent `HostShare`
records.
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

Relay identity, authorization, catalog, and structured audit metadata are
PostgreSQL-only. PTY output, input, prompts, terminal snapshots, file contents,
and raw secrets are never persisted by the relay or included in its logs or
audit records.

State recovery uses one host-scoped subscription: subscription-first
registration is followed by an atomic snapshot of every active share, with one
epoch, sequence, and watermark for that host link. The daemon keeps no relay
replay log. Any gap, overflow, epoch change, malformed event, or reconnect
causes the relay to discard the live cache and perform a bounded full snapshot.
Relay outages do not stop host sessions or affect local and NetBird owner
operation; revoked-share sessions continue permanently as owner-only.

## Implementation references

The complete contract is in
`docs/design/team-relay-control-plane-rfc.md`; use the
[source map](../assistant/source-map.md) to locate it in a source checkout. The
umbrella is [#56](https://github.com/zajca/pohunek/issues/56); implementation is
split among host identity [#81](https://github.com/zajca/pohunek/issues/81),
relay foundation [#85](https://github.com/zajca/pohunek/issues/85), transport
[#72](https://github.com/zajca/pohunek/issues/72), protocol v4
[#70](https://github.com/zajca/pohunek/issues/70), shares
[#82](https://github.com/zajca/pohunek/issues/82), session authorization
[#83](https://github.com/zajca/pohunek/issues/83), synchronization
[#84](https://github.com/zajca/pohunek/issues/84), relay routing
[#71](https://github.com/zajca/pohunek/issues/71), clients
[#86](https://github.com/zajca/pohunek/issues/86), and operations
[#87](https://github.com/zajca/pohunek/issues/87). Until those issues land,
`docs/public-api.md` is authoritative for shipped v3 behavior.
