---
type: Concept
id: concept/architecture
title: Universal assistant architecture
description: The assistant is one ordinary agent session guided by a materialized knowledge bundle, a redacted snapshot, and a small navigational prompt.
source_kind: manual
intents: [setup, project, update, debug, help]
---

# Universal Assistant Architecture

The Universal Pohunek Assistant is designed as one capable coding-agent session,
not as a separate runtime or a set of specialized agents. The launch command
materializes a version-matched knowledge bundle, writes a redacted live snapshot,
builds a short navigational prompt, and starts a normal PTY-backed session with
initial input. Like every managed live session, that PTY belongs to its isolated
`pohunek-sessiond` worker; restarting the public daemon reconnects to the same
assistant runtime rather than relaunching it.

Knowledge delivery is pull-by-file. The prompt points the agent at this bundle,
the snapshot file, and the [source map](../assistant/source-map.md); it does not
inline the whole corpus. That keeps prompt size bounded and lets the same
Markdown serve humans and agents.

The planned assistant command surface uses one implementation with intent
filters: setup, project, update, debug, and help. Intent changes the initial
table of contents and first-step steering, but the concepts and safety model are
shared.

Local launches can bootstrap or verify the daemon before starting a session.
Remote launches preserve the existing remote session safety model and require a
knowledge bundle materialized on the host that runs the agent.

All shipped clients use the same public protocol v3. Each request advertises an
inclusive `minimum`/`maximum` version range; the first response selects the
highest overlap for that connection. The old integer-v1 request envelope is not
accepted. Waiting observation calls open dedicated connections so they do not
block the caller's ordinary control connection.

The shipped platform foundation centralizes target-neutral process identity,
kernel Unix peer identity, and native service supervision contracts in
`crates/platform`. Every process-identity consumer in the daemon and the worker
resolves one host inspector from those contracts rather than parsing operating
system records on its own. Linux uses the real procfs, pidfd, peer-credential,
and systemd transient-unit backends; Darwin uses `libproc` process records, `sysctl`
`KERN_PROCARGS2` argument regions, and a kqueue `EVFILT_PROC`/`NOTE_EXIT` exit
watch. Both backends read the same facts: process, parent, and process-group
ids, an opaque same-boot start identity, the controlling-terminal foreground
process group, the executable path, the working directory, and only the
allowlisted `POHUNEK` ownership markers. A process that has exited but is not
yet reaped keeps its identity on both backends and is reported as no longer
running. Argument and environment regions stay
separate, so an environment value is never argument evidence and an argument is
never an ownership marker. An observation that fails is an explicit typed
failure, never a healthy absence, and never authorizes a mutation.

Darwin adds a privilege boundary Linux does not have: the kernel serves most
process records only to the owner of the target process and refuses everyone
else, so ownership is settled first through the short process record, the one
record it serves for every process id. A process owned by another user is
therefore outside the same-user contract and reported as absent, while a
privileged fact refused for a process the caller does own stays a denial. Three
facts need a privilege pohunek does not ask for and so are unavailable on macOS:
the working directory of another user's process, that process's argument vector,
and the environment of a code-signing-restricted process, whose environment
region the kernel omits even for the owner. A process whose environment cannot be
read reports no ownership markers, which keeps it observable rather than
adoptable. An argument vector is an optional fact for the same reason: a process
that has not finished its `exec`, or whose address space is being replaced or
torn down while the kernel copies it out, is reported without a command line
rather than failing the inventory it appears in. The shared
secure-path contract preserves XDG config/data/state/cache precedence and adds a
short, owner-private macOS runtime default without moving durable host identity.
Native Apple Silicon CI compiles and tests these shared contracts with a macOS
14 deployment target, but this does not mean complete macOS host or client
support is available. Intel Macs are outside the current release scope. Darwin
kernel peer identity and the session worker's portable PTY readiness are in
place: the worker waits for PTY output with one `poll(2)` implementation shared
by both targets, and its complete test suite runs natively. The daemon and CLI
build and test natively on macOS too. Client and WebUI integration, signed
artifacts, and the remaining native acceptance stay explicitly deferred through
issues #101-#105 under the `Complete macOS support` milestone and macOS
Project. The issue hierarchy, milestone, and Project own delivery scope,
sequencing, and status; the accepted macOS RFC records design constraints
rather than live tracking state.

Session workers are native service jobs, one per worker generation, supervised
by the same lifecycle engine on both targets: systemd transient units
(`pohunek-<ns>-worker-<session-id>-<generation>.service` in
`pohunek-<ns>-sessions.slice`) on Linux, and launchd jobs
(`io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`) whose
definitions stay in a private state directory on macOS. The daemon is the only
login service (`pohunek service install`). `<ns>` is derived from the user ID
and the canonical state and runtime roots, so separate installations never
adopt or retire each other's jobs. Workers live for the login session: closing
a terminal or locking the screen is safe, while logout or reboot ends them and
the next login reports those sessions `lost` without restarting them.

## Owner paths and the accepted relay direction

Current protocol-v3 operation is owner-only. Local clients connect to the Unix
socket, direct remote clients use a configured overlay such as NetBird, and the
shipped Bun browser backend transparently maps one WebSocket to one daemon
connection. Each host daemon remains authoritative for its sessions and each
worker remains authoritative for one live PTY. This owner WebUI remains a
supported local/direct-overlay path after the team relay ships.

Pohunek has an [optional team-relay design](team-relay.md) with an implemented
reduced foundation. The PostgreSQL-backed relay provides fencing, recovery,
protected initial provisioning, generic OIDC browser/device authentication, and
bounded HTTPS account and credential lifecycle. Standalone and direct NetBird
modes remain independent. Host links, `HostShare`, session origin, routing,
attach, team administration, and the team WebUI remain deferred. The relay has
no local mode; its future team WebUI and the retained owner WebUI use separate
explicit API adapters, credentials, state, and origins.

Protocol v4 and the typed host/team API will arrive only through their linked
implementation issues. The current bounded foundation API and native relay CLI
do not imply a host or team surface. Assistants must not infer future commands
or fields from the RFC; verify currently available behavior in
`docs/public-api.md` through the [source map](../assistant/source-map.md).

The planned relay contract is normative only in the RFC sections on identity,
recovery, scheduling, resources, snapshots, catalog, audit, and dependencies.
Its implementation path is completed reduced #85 → #107/#108/#92 → complete
#72 → #70/#82/#83/#84/#71 → #86 and #87, then
post-release #73/#88. The first relay release trusts collaborators at the host
Unix-account boundary; its ACLs do not provide workload isolation.

Related concepts:

- [Sessions](sessions.md)
- [Projects](projects.md)
- [Worktrees](worktrees.md)
- [Agent profiles](agent-profiles.md)
- [Optional team relay](team-relay.md)
- [Trust model](../safety/trust-model.md)
