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

Materialization publishes the bundle only after its files and directories have
been synchronized, then synchronizes the cache directory after the atomic rename.
A synchronization error fails the operation; retrying completes the publication
before it reports success. Each attempt also synchronizes the cache ancestry that
may have been created by an earlier failed attempt, including when reusing a
complete bundle. An interrupted, incomplete version is moved aside
inside the private cache and rebuilt. Symbolic links and unsafe ownership or
permissions are refused rather than followed or repaired outside that cache.

The daemon and local assistant serialize publication and cache cleanup with the
same materialization lock. Cleanup preserves active temporary extractions and
collects abandoned ones after their ownership lock is released. Legacy temporary
directories without an ownership lock are retained for at least a day before
cleanup under the shared lock; a live legacy owner is retained conservatively.
Cleanup is best effort and does not turn a successful publication into a failure.

The planned assistant command surface uses one implementation with intent
filters: setup, project, update, debug, and help. Intent changes the initial
table of contents and first-step steering, but the concepts and safety model are
shared.

Local launches can bootstrap or verify the daemon before starting a session.
Remote launches preserve the existing remote session safety model and require a
knowledge bundle materialized on the host that runs the agent.

All shipped clients use the same public protocol v4. Each request advertises an
inclusive `minimum`/`maximum` version range; the first response selects the
highest overlap for that connection. A daemon accepts its own version and the
previous one (`3..=4` today) through shape-only adapters, while clients advertise
only their own version. The old integer-v1 request envelope is not accepted. Waiting observation calls open dedicated connections so they do not
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
failure, never a healthy absence, and never authorizes a mutation. On Darwin a
live process between images can expose an argument region that is absent,
uncopyable, or not yet laid out; that process is still listed, without a
command line, and its ownership markers read as unobservable rather than
unmarked, so the runtime sweep skips it as unreadable and never signals it
while the rest of the process table is still classified. Darwin also reports the
kernel's boot-wide creation number of every same-user process and of the process
that created it. The recorded creator keeps its identity after it exits, until
the process re-executes after being reparented, when the kernel rewrites it to
launchd's number, so a launchd creator proves nothing. When the sweep knows the
lost worker's creation number, that lineage decides a process whose environment
the kernel withholds: a descendant of the worker is reaped, a process created
before the worker or under an older non-launchd creator is ignored, and every
other one (a re-executed orphan, creator gone) stays skipped as unreadable. When
the sweep also knows the worker's coalitions, a skipped process that the kernel
puts in a resource and a jetsam coalition that both differ from the worker's,
such as a job launchd started after the worker, is ignored as well: a descendant
keeps the worker's coalitions through every fork, exec and reparenting.

A worker also journals `boot_session_id` (the boot session UUID on macOS, the kernel boot id on Linux; only the canonical UUID shape counts). After a host reboot the daemon sees another boot than the journal and treats the runtime as ended without scanning, so the session leaves `running` with the lost runtime; a journal without the field (a worker of 0.38.x or earlier) keeps the unconfirmed outcome.

Linux decides such a process by its cgroup. The worker journals the unified
cgroup path it runs in as the optional `worker_control_group` (no schema bump,
used only in the boot it was recorded in). A skipped process whose own `0::` path
lies outside the `user@<uid>.service` subtree of that path, such as a Docker
scope or a logind session scope, is ignored and not reported: cgroup v2 lets an
unprivileged process migrate only within the user manager's subtree, because
everything above it is root-owned. A process inside the subtree (a user unit,
`app-*.scope`, `systemd-run --user --scope`) proves nothing and keeps the
cleanup unconfirmed. The evidence only excludes: it never selects or signals a
process, and a readable marker or an older start time decides first. A journal
without the field, a malformed path, another boot or an unreadable record give no
proof, and so does a daemon or worker outside the initial cgroup namespace,
whose paths are relative to another root. A same-user process with
`CAP_DAC_OVERRIDE` or `CAP_SYS_ADMIN` through a file capability can migrate
anywhere, and a root-owned service that starts same-user jobs with a recorded
copy of the submitter's environment (for example `at(1)`) runs them outside the
subtree, so they count as foreign; the marker scheme does not defend against a
descendant that drops its markers either.

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
14 deployment target, and macOS on Apple Silicon is a supported platform.
CI and release acceptance run on macOS 15 only, and Intel Macs are outside
the current release scope. Darwin
kernel peer identity and the session worker's portable PTY readiness are in
place: the worker waits for PTY output with one `poll(2)` implementation shared
by both targets, and its complete test suite runs natively. The daemon and CLI
build and test natively on macOS too. Release archives are ad-hoc
signed and not notarized. The open items of #100 and #102-#105 are ordinary
follow-ups tracked under the `Complete macOS support` milestone and macOS
Project. The issue hierarchy, milestone, and Project own
delivery scope, sequencing, and status; the accepted macOS RFC records design constraints
rather than live tracking state.

Session workers are native service jobs, one per worker generation, supervised
by the same lifecycle engine on both targets: systemd transient units
(`pohunek-<ns>-worker-<session-id>-<generation>.service` in
`pohunek-<ns>-sessions.slice`) on Linux, and launchd jobs
(`io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`) whose
definitions stay in a private state directory on macOS. The daemon is the
login service of pohunek itself (`pohunek service install`); the only other
login units are the jobs and services of enabled workflow plugins
(`pohunek-<ns>-wf-*` units, `io.github.zajca.pohunek.<ns>.wf.*` agents), which
run the CLI's `pohunek workflow __exec` trampoline, never the daemon, and which
worker discovery never adopts (see the
[workflow plugins guide](../guides/workflow-plugins.md#jobs-and-services)).
`<ns>` is derived from the user ID
and the canonical state and runtime roots, so separate installations never
adopt or retire each other's jobs. Workers live for the login session: closing
a terminal or locking the screen is safe, while logout or reboot ends them and
the next login reports those sessions `lost` without restarting them.

## Owner paths and the accepted relay direction

Current protocol-v4 operation is owner-only. Local clients connect to the Unix
socket, direct remote clients use a configured overlay such as NetBird, and
external clients such as a browser control center can map one WebSocket to one
daemon connection through their own relay. Each host daemon remains
authoritative for its sessions and each worker remains authoritative for one
live PTY. The owner path remains supported after the team relay ships.

Pohunek core ships no user interface. GUIs, the web control center, and desktop
launchers are external clients developed in `zajca/pohunek-work`. They use public
contracts only: the CLI with `--json`, or the public protocol through the Rust
crates (pinned by git tag) and the TypeScript SDK (release tarballs pinned by URL
and integrity). They pin the protocol version they were built against (a daemon also serves the
previous version), and the core
crates they link are a pinned, not a stable, API. Issue and PR providers live
only in `zajca/pohunek-work`.

Pohunek has an [optional team-relay design](team-relay.md) with an implemented
reduced foundation. The PostgreSQL-backed relay provides fencing, recovery,
protected initial provisioning, generic OIDC browser/device authentication, and
bounded HTTPS account and credential lifecycle. Standalone and direct overlay
modes remain independent. Host links, `HostShare`, session origin, routing,
attach, team administration, and the team WebUI remain deferred. The relay has
no local mode; its future team WebUI and any owner-path browser client use
separate explicit API adapters, credentials, state, and origins.

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
