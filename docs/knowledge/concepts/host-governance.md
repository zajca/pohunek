---
type: Concept
id: concept/host-governance
title: Host identity and local governance
description: Understand the shipped host-local stable identity, safe governance inspection, and owner-private persistence boundary.
source_kind: manual
intents: [setup, debug, help]
---

# Host Identity and Local Governance

Protocol v3 ships a host-local foundation for stable identity and governance.
It does not ship a relay connection, relay-local mode, team UI, share API, or a
public enrollment, owner-transfer, or recovery command. The accepted optional
team-relay design remains future work; use only current owner paths until the
owning issues ship.

## Inspect the safe public state

Use the read-only command on the host route you want to inspect:

```sh
pohunek host governance inspect local --json
pohunek host governance inspect <host> --json
```

The command uses the same owner-only Unix or direct configured-overlay route as
other v3 calls. It is safe to use through the Rust client, the generated
TypeScript method map, the native GUI, or the transparent owner WebUI transport.
It does not grant a relay or browser any new authority.

The response always has a stable opaque `host_id` and an
`approval_key_reference`. `HostId` is not a hostname, IP address, NetBird peer,
daemon route selector, or a value that may be substituted for a relay, principal,
or team identifier. Treat every typed ID as opaque and preserve its canonical
form.

A never-enrolled host reports `enrollment`, `owner`, `owner_revision`, and
`quarantine` as `null`, while retaining the stable host ID and safe key
reference. An enrolled host has exactly one enrollment, exactly one tagged
owner (`principal` or `team`), and non-zero canonical decimal enrollment and
owner revisions. Co-ownership is not a state. Quarantine is present exactly
when the enrollment is quarantined.

The response intentionally excludes approval private material, proposal and
outcome data, signatures, nonces, retired state, relay credentials, and all
other sensitive persistence fields. Do not infer those values from a stable ID
or write tooling that expects them in `--json` output.

## Local lifecycle and persistence

The durable local lifecycle records disabled, pending local commit, active,
rotating, quarantined, and locally unenrolled enrollment states. A host has zero
or one current enrollment; owner and enrollment revisions are checked rather
than silently wrapping. The local state can retain a bounded retired enrollment
summary to reject stale future coordination. That summary is not part of safe
inspection.

Local owner-transfer primitives bind exact host, relay, owner, revision,
proposal, nonce, expiry, suspension intent, and approval-key coordinates before
the daemon persists an outcome. They are internal host-local contract data, not
an operator-facing mutation API. Do not invent an enrollment, transfer, or
unenroll command from this implementation.

The records live under `$XDG_STATE_HOME/pohunek/host/` (or
`~/.local/state/pohunek/host/`) with the stable identity, private approval key,
governance record, and a cross-process lock. The daemon validates owner-private
paths and modes, serializes writers, and atomically replaces records. If a
replacement was renamed but cannot be proven durable, it treats the repository
as unavailable until an explicit serialized reload; do not treat a reread as
proof that the update is safe.

Local unenrollment does not stop a PTY, remove the owner Unix socket, disable a
direct NetBird/WireGuard owner route, or prevent the native GUI and transparent
owner WebUI from continuing to inspect and operate their existing owner
surfaces.

## Doctor and recovery

`pohunek doctor --json` includes host-governance durability, stable identity,
private-storage, consistency, approval-key, and quarantine checks. A warning or
failure is a reason to inspect the daemon's typed result and owner-private state
through normal recovery procedures; do not edit state files, key material, or
lock files by hand. A governance failure must not be replaced with a fabricated
last-known owner or revision. Protocol v3 has no public governance recovery or
mutation command.

Exact signed projection evidence can restore only a
`ProjectionConflict` quarantine. It restores the persisted pre-quarantine
lifecycle rather than assuming `active`. `HostIdentityClone` and
`EnrollmentConflict` remain quarantined until explicit local unenrollment or a
future authoritative resolution; neither outcome changes existing local Unix or
direct NetBird owner access, live sessions, or PTYs. The relay has no local mode
and no authority over those owner paths.

For daemon startup and storage diagnostics, use [debug daemon](../runbooks/debug-daemon.md).
For future relay boundaries, read [optional team relay](team-relay.md); it is
not an instruction to configure a relay today.
