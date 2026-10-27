# pohunek Architecture

This document describes the shipped owner architecture and the accepted
optional team-relay direction for `pohunek`.

## Status and Scope of This Revision

`idea.md` captures the original, broad product brainstorm. This document is the
**authoritative current direction**. Where this document and `idea.md` disagree,
this document wins. Detailed relay decisions are authoritative in the
[accepted team-relay RFC](design/team-relay-control-plane-rfc.md), tracked by
[#56](https://github.com/zajca/pohunek/issues/56).
The accepted complete macOS contract lives in the
[macOS support RFC](design/macos-support-rfc.md). Its delivery source of truth is
the issue hierarchy rooted at [#94](https://github.com/zajca/pohunek/issues/94),
the [`Complete macOS support` milestone](https://github.com/zajca/pohunek/milestone/2),
and the [macOS project](https://github.com/users/zajca/projects/4).

Pohunek has two explicit trust domains:

- **Owner paths, shipped today.** One operator controls a host through its
  owner-private Unix socket or directly through a configured private overlay.
  NetBird is the default overlay and Tailscale a supported opt-in overlay. These
  paths require no central service and remain supported when no relay is
  configured.
- **Optional team relay, reduced foundation implemented.** `pohunek-relayd`
  provides PostgreSQL-backed fenced authority, recovery, protected local
  provisioning, generic OIDC authentication, and bounded HTTPS account and
  credential operations. Host enrollment, shares, routing, attach, team clients,
  and team administration remain deferred. Enrolling does not replace or require
  an overlay.

The following invariants span both domains:

- **Host authority remains local.** Each `pohunekd` is authoritative for its own
  logical sessions, workers, PTYs, processes, worktrees, and host policy.
- **Remote owner transport is direct over the configured overlay** (NetBird by
  default, Tailscale opt-in), not an SSH bridge or relay
  fallback.
- **Agents run PTY/TUI-first** in real terminals. Pohunek does not re-render an
  agent-specific protocol as its execution model.
- **Owner discovery is tokenless and overlay-local**, with live capability
  queries instead of signed manifest exchange.
- **Provider integration remains outside the daemon.** Client surfaces use
  `gh` or Linear GraphQL; future relay provider delivery is separately tracked
  by [#73](https://github.com/zajca/pohunek/issues/73).

### Implementation status

| Area | Status | Owner |
|---|---|---|
| Standalone Unix-socket owner operation | Shipped in public protocol v4 | Existing daemon, CLI, and SDK |
| Direct configured-overlay operation, including NetBird | Shipped in public protocol v4 | Existing daemon and clients; generic overlay work completed in [#69](https://github.com/zajca/pohunek/issues/69) |
| Shared Linux/macOS platform, secure path, and portable filesystem contracts with native Darwin CI | Shipped; Linux and Apple Silicon macOS (deployment target 14.0, tested on macOS 15) are supported platforms | [#95](https://github.com/zajca/pohunek/issues/95)-[#96](https://github.com/zajca/pohunek/issues/96); open macOS follow-ups [#100](https://github.com/zajca/pohunek/issues/100) and [#102](https://github.com/zajca/pohunek/issues/102)-[#105](https://github.com/zajca/pohunek/issues/105) |
| Local/direct-overlay browser client transport | External client in `zajca/pohunek-work`; the SDK ships the `WsTransport` client and a loopback test relay | Team web mode is separate work in [#86](https://github.com/zajca/pohunek/issues/86) |
| Stable host identity, one exact principal-or-team owner, checked revisions, local lifecycle, and safe v4 inspection | Shipped host-local foundation; no relay API or mutation surface | [#81](https://github.com/zajca/pohunek/issues/81) |
| Rust relay foundation: PostgreSQL, lease fencing, recovery quarantine, protected initial Owner/service-account provisioning, generic OIDC, HTTPS auth/account/credential operations, provider-neutral account linking, and native credential CLI | Implemented reduced foundation; no host link, routing, attach, or team administration API | [#85](https://github.com/zajca/pohunek/issues/85) and [#107](https://github.com/zajca/pohunek/issues/107); follow-up [#108](https://github.com/zajca/pohunek/issues/108), [#92](https://github.com/zajca/pohunek/issues/92), and [#86](https://github.com/zajca/pohunek/issues/86) |
| Verified Keycloak-brokered social identity and bounded external eligibility | Accepted, not implemented; blocks completion of transport enrollment | [#92](https://github.com/zajca/pohunek/issues/92) |
| Host-initiated userspace WireGuard link | Accepted, not implemented; transport completion follows #92 | [#72](https://github.com/zajca/pohunek/issues/72) |
| Public protocol v4 relay host link, `SessionOrigin`, and daemon share guards | Accepted, not implemented | [#70](https://github.com/zajca/pohunek/issues/70) |
| Locally approved `HostShare` and relay-side session ACLs | Accepted, not implemented | [#82](https://github.com/zajca/pohunek/issues/82), [#83](https://github.com/zajca/pohunek/issues/83) |
| Atomic snapshot/watermark synchronization without replay | Accepted, not implemented | [#84](https://github.com/zajca/pohunek/issues/84) |
| Relay routing, aggregation, attach proxy, host/team API, and team clients | Accepted, not implemented; the bounded foundation HTTPS API is not a host or team surface | [#71](https://github.com/zajca/pohunek/issues/71), [#86](https://github.com/zajca/pohunek/issues/86), [#87](https://github.com/zajca/pohunek/issues/87) |
| Delegated task layer (`task.*`, causal settlement, worker evidence journal, checks, OpenCode adapter) | Accepted, not implemented; design in `docs/design/delegated-task-runs-rfc.md`; amended 2026-10-08 with `task.input_control` human control (8.9) and the client work-item/acceptance contracts (16.5, 16.6) | [#182](https://github.com/zajca/pohunek/issues/182) and its sub-issues #219–#232 |
| Relay dark factory (task ACLs, delegation budgets, escalation, factory client contract) | Accepted, not implemented; design in `docs/design/relay-dark-factory-rfc.md`; amends the relay RFC via [#233](https://github.com/zajca/pohunek/issues/233); amended 2026-10-08 with protected audit reserves (8.5), the FactorySpec run manifest (12.5) and bounded progress policy (12.6) | [#185](https://github.com/zajca/pohunek/issues/185) and its sub-issues #233–#240 |

## Goals

- Provide a CLI-first control plane for durable coding-agent work across your own
  machines, locally or through a configured overlay such as NetBird.
- Keep every meaningful workflow available through `pohunek` commands with
  human-readable defaults and machine-readable `--json` output.
- Keep standalone and direct-overlay owner operation independent of any central
  application service.
- Keep owner-path clients (including the external browser control center)
  available for local-daemon and direct-overlay access independently of the
  team relay.
- Ship no user interface in core: the deliverables are the daemon, session
  worker, CLI, relay, Rust crates, and TypeScript SDK; every UI and launcher is
  an external client (see "Client boundary").
- Make each host authoritative for its own PTYs, agent processes, state, logs,
  and worktrees.
- Support durable detach and reattach by giving every live session a dedicated
  worker whose PTY and child lifecycle is independent of the restartable daemon.
- Use real PTY/TUI agent sessions for Codex, Claude Code, and the local
  interactive Hermes Agent backend, with agent
  state derived from OSC terminal titles first, screen-content pattern matching
  as fallback, and PTY activity for the working signal. Hooks capture only the
  native session ID for resume (Codex/Claude Code do not report live state via
  hooks). The accepted delegated task runs RFC extends this for task
  sessions: managed task-evidence hooks report bounded, field-allowlisted
  turn evidence to the worker socket (accepted, not implemented;
  [#182](https://github.com/zajca/pohunek/issues/182)).
- Be agent-operable: an operator agent can drive the whole tool through the same
  `--json` CLI and subscription API a human uses.
- Add an optional multi-team relay whose authority never exceeds locally
  approved host shares and which does not publish owner-path sessions.

## Non-Goals

- Requiring the relay for local or direct-overlay owner operation.
- Moving PTY, process, worktree, or host-policy authority into the relay.
- Publishing local or direct-overlay sessions to teams, even after host
  enrollment.
- Protecting a host from a compromised trusted relay within the union of its
  active `HostShare` ceilings.
- Treating direct-host execution under the daemon owner's Unix account as a
  hostile-workload sandbox. Container and VM execution profiles are tracked by
  [#88](https://github.com/zajca/pohunek/issues/88).
- SSH bridging as the remote transport (direct overlay transport replaces it).
- A cryptographic owner mesh: signed manifests and replicated mesh state.
- In-tree provider adapters in the core path (shell-out instead).
- ACP as the first agent runtime (deferred; PTY/TUI-first).
- WebSocket as the core daemon protocol.

## High-Level Architecture

### Shipped owner paths

```text
  CLI (local)                         CLI (remote)
       |                                   |
       | Unix socket                       | TCP over the configured overlay
       | (resolved private runtime root)   | (daemon binds ONLY to the overlay IP)
       v                                   v
 +-----------------------------------------------------------+
 |               host control plane (pohunekd)               |
 | public protocol | logical state | events | reconciliation  |
 +-----------------------------------------------------------+
       |
       | private owner-only Unix protocol
       v
 +-----------------------------------------------------------+
 |  pohunek-sessiond: one native job per worker generation    |
 |  (systemd transient unit on Linux, launchd job on macOS)   |
 |  PTY master | child handle | output ring | terminal tracker |
 +-----------------------------------------------------------+
       |
   Codex / Claude Code / Hermes Agent running in worker-owned PTYs
       |
   identity hooks --> worker; notifications --> daemon
```

The host-local Pohunek service is authoritative for live work. `pohunekd` is the
logical-session authority and public control plane; a `pohunek-sessiond` worker
is authoritative for one live PTY generation. Owner paths have no shared mesh
state or coordinator. A remote owner reaches a host daemon directly over
the configured overlay using the same public protocol as the local Unix socket. Workers are
never remotely addressable.

### Client boundary

This repository ships no user interface. GUIs, the web control center, and
desktop launchers live in `zajca/pohunek-work` and reach core through public
contracts only: the CLI with `--json`, and the public protocol through the
versioned SDKs. The Rust crates are pinned by git tag; the TypeScript SDK is
pinned by release-tarball URL and integrity. The core crates a Rust client links
(`client`, `protocol`, `paths`, `platform`, `prompt`, `knowledge`, `assistant`,
and the `netbird`, `overlay` and `overlay-tailscale` crates that `client` links
for the overlay transports) are a pinned, not a stable, API: they stay pre-1.0
with no back-compat shims.
External UIs pin the `PROTOCOL_VERSION` they were built against; a daemon one
release newer still serves them through the protocol window, and the SDK talks
to a daemon one release older through its own adapter (see "Protocol
versioning"). Methods only UI clients
call (`host.discover`, `worktree.remove`) are still public obligations with
server-side contract tests in core (`docs/public-api.md`, "External clients").
Issue and PR providers (Linear, GitHub) are client concerns and never enter
core.

Herd orchestration stays outside the daemon. A herd (a manager session that
launches and supervises a set of worker sessions) is defined, limited and
orchestrated by manager tooling or by a plugin, through the public contracts.
Core keeps only two primitives for it, and no herd definition, limit or
admission policy: the `spawned_by` attribution of a session
([#1300](https://github.com/zajca/pohunek/issues/1300)) and durable session
metadata ([#1556](https://github.com/zajca/pohunek/issues/1556)); neither has
shipped yet. Metadata stays opaque to the daemon, and attribution is never an
authorization input.

### Shipped host identity and local governance

Every daemon bootstraps one stable, opaque `HostId`. It is not a hostname,
address, daemon instance, overlay identity, or a selector for a transport
route. The protocol keeps `HostId`, `RelayId`, principal IDs, and team IDs as
separate validated types, so a value of one kind cannot be substituted for
another on the wire.

The daemon persists one owner-private host-governance record. Its safe public
projection always includes the `HostId` and an approval-key reference. It has
either no enrollment and no owner, or exactly one enrollment with exactly one
tagged owner (`principal` or `team`) and non-zero, checked enrollment and owner
revisions. It never represents co-ownership. Quarantine is explicit and is
present only for a quarantined enrollment. The local lifecycle records disabled,
pending-local-commit, active, rotating, quarantined, and locally-unenrolled
states; a locally unenrolled record may retain the bounded retired summary
needed to reject stale future coordination without exposing it through the
public inspect response.

The local record also has the primitives for exact owner-transfer proposals and
signed outcomes. They bind the host, relay, current and target owner, revisions,
proposal identity, nonce, expiry, suspension intent, and approval-key reference.
They are durable local contract data, not public relay operations: protocol v4
exports neither proposal/outcome material nor a mutation method.

Host state is separate from session metadata under the owner's XDG state tree:
`$XDG_STATE_HOME/pohunek/host/` (or `~/.local/state/pohunek/host/`). The stable
identity record, private approval signing key, governance record, and process
lock are owner-private. Startup takes the instance lock before opening this
state. Descriptor-relative validation, exact modes, cross-process locking,
atomic replacement, and file/directory synchronization protect it against
unsafe paths and torn writes. If replacement has committed but durability is
uncertain, the daemon discards the in-memory repository and refuses governance
use until an explicit serialized reload; a visible reread is not treated as
proof of durability. The retained, validated host-state directory is the lock
authority that serializes cooperating daemon writers; record and lock-marker
bindings are validated against that directory. This does not claim protection
from hostile same-UID control or replacement of the complete XDG state root:
that remains within the owner trust domain.

This host-local foundation does not change session ownership. Local Unix,
direct overlay owner access and owner-path browser clients
remain available. Local unenrollment does not stop PTYs, revoke those
owner paths, or create a relay-local mode.

### Deferred optional relay topologies

The relay design adds two topologies without changing the two owner topologies:

```text
1. Standalone

owner client -- Unix socket --> pohunekd --> pohunek-sessiond

2. Direct owner mesh

owner client -- overlay TCP --> pohunekd --> pohunek-sessiond

3. Standalone host plus optional relay

team client -- HTTPS/WSS --> pohunek-relayd
                                  ^
                                  | host-initiated userspace WireGuard + TCP
                                  |
                              pohunekd --> pohunek-sessiond

4. Direct owner mesh plus optional relay enrollment

owner client -- NetBird TCP ----------------------+
                                                   v
team client -- HTTPS/WSS --> pohunek-relayd <--- pohunekd
                                                   |
other owner clients -- NetBird TCP --> other pohunekd hosts
```

NetBird is neither replaced nor required by the relay. The relay publishes a
public WireGuard UDP endpoint and discovers no host from NetBird: `pohunekd`
enrolls explicitly and initiates the userspace WireGuard tunnel, long-lived
control connection, and every attach connection. The relay never opens a
connection to a host. The existing `pohunekd` binary
contains the future connector; there is no sidecar. Both endpoints run
WireGuard and bounded IPv4/TCP in process, with no kernel interface, system
route, privileged helper, or `CAP_NET_ADMIN`. This transport belongs to
[#72](https://github.com/zajca/pohunek/issues/72).

The implemented relay foundation is a PostgreSQL-backed, fenced HTTPS authority
for generic authentication and credential lifecycle. The planned production
relay extends it with routing, aggregation, team authorization, audit, and quota
authority. The
external owner-path browser gateway (maintained in `zajca/pohunek-work`)
remains the production gateway for its local daemon and direct-overlay peers.
It is not a relay authority and is not replaced by [#86](https://github.com/zajca/pohunek/issues/86). The Rust relay has
no local mode: its future typed team surface will remain separate. The two web
surfaces may share presentation components, but they use explicit origins,
transports, credentials, and state without fallback. The relay does not own PTYs
or durable terminal content.

### Accepted relay model concepts

- The shipped host-local identity/governance foundation gives a future relay a
  stable opaque `HostId`, one exact principal-or-team owner, at most one local
  enrollment record, checked revisions, and locally confirmed transfer
  coordinates. It does not create a relay connection, public enrollment API,
  or team surface. The relay-facing behavior remains future work.
- Relay enrollment and human CLI authorization use OIDC device flow; browser
  login uses Authorization Code with PKCE. There is no loopback login fallback.
  Keycloak is the pinned reference broker for Google and GitHub, while Pohunek
  remains provider-neutral. Verified external eligibility expires within 60
  minutes of upstream verification and refreshes, activity, restart, or provider
  uncertainty never extend it. [#92](https://github.com/zajca/pohunek/issues/92)
  owns the end-to-end evidence flow before #72 can complete.
- `HostShare` is a revisioned, default-deny, locally approved ceiling binding a
  host to one team. It restricts operations, registered projects and canonical
  worktree roots, owner-authored profiles, resource limits, and future execution
  backends. A host may approve multiple independent shares for multiple teams.
  [#82](https://github.com/zajca/pohunek/issues/82) owns the lifecycle and policy.
- `SessionOrigin` is immutable host-authored state: `LocalOwner`,
  `DirectOverlayOwner`, or `Relay { relay_id, host_share_id }`. Relay operations
  can reach only sessions created through the matching active share; local and
  direct-overlay sessions never enter relay snapshots and cannot be published
  later. Owner paths retain authority over relay-created sessions. [#70](https://github.com/zajca/pohunek/issues/70)
  owns the v4 wire and daemon guard framework.
- End-user identities, service accounts, teams, groups, roles, and session ACLs
  belong only to the relay. `pohunekd` authenticates the enrolled relay and
  enforces `HostShare` plus `SessionOrigin`; opaque attribution metadata is not
  an authorization input. [#83](https://github.com/zajca/pohunek/issues/83) and
  [#85](https://github.com/zajca/pohunek/issues/85) own relay authorization.

Only relay-created sessions for active shares synchronize. The complete
single-subscription, ordered-writer, bounded snapshot, cancellation, fairness,
and reconnect contract is normative in RFC §§11 and 16. A gap, overflow, epoch
change, malformed event, or disconnect discards the live cache and triggers a
bounded full snapshot; snapshots are not offline history. During relay or share
outages, sessions keep running and owner paths remain available. Revoked-share
sessions become owner-only without being stopped. [#84](https://github.com/zajca/pohunek/issues/84)
owns this recovery contract.

## Host Daemon

### Platform boundary

`crates/platform` defines the narrow operating-system contracts shared by the
daemon and session worker. Process facts use a PID plus opaque same-boot start
identity, with a separate boot identity where persistence crosses reboots.
Kernel peer identity is captured in the platform backend while authorization
policy stays with the accepting service. It has two halves: the peer's owner is
connection-time identity that both kernels freeze when the connection is
established, while the peer process id is inspection-time identity, because
Darwin serves it from the peer socket's last operating process rather than from
a connect-time snapshot. A service therefore captures the peer before parsing a
request and re-reads the kernel answer before each decision that grants
authority, so a descriptor handed to another process is rejected rather than
inherited. A peer the kernel cannot attest, or one without a process id, is an
explicit rejection; neither a request field nor the owner alone can stand in for
one.

Native service supervision registers one explicit, validated job definition
(absolute versioned executable, argv, bootstrap environment, logs, timeouts,
restart policy, open-file limit) per worker generation and exposes validated
logical IDs, portable states, proven executable and arguments, and stable
process identities through start, bounded discovery, inspect, and retire
operations. Linux implements that contract through systemd transient units over
D-Bus; macOS implements it through `/bin/launchctl` `bootstrap`, `bootout`, and
`print`, keyed only on exit statuses and never on printed text. Session code
never sees unit names, labels, D-Bus paths, or raw `MainPID` semantics. Worker
readiness and authority still come from the private worker handshake.

A separate plugin-unit contract installs persistent workflow plugin units, each
keyed by plugin, kind, and name: a periodic job or a long-running service
restarted after every exit. A job's runs never overlap and start about one
interval apart. The interval is awake time counted from the unit's activation
within one service-manager lifetime: it pauses while the host sleeps (launchd
skips a firing due during sleep) and starts over at every login, so
wall-clock cadence (catching up a daily job on a host that is never awake
that long) belongs to the caller, which keeps its own last-run record and
skips runs that are not due yet. Intervals, timer
accuracy, and restart delays are whole seconds of at least one second, the
granularity both native managers express. The native run bound and restart
delay are backstops; the caller enforces the real run and restart policy.
On systemd the plugin units use `KillMode=mixed` (worker and daemon units keep
`control-group`): a stop signals only the unit's main process, which stops
its own descendants within its grace, as launchd does, and the final
`SIGKILL` still reaches every process left in the unit.
Each unit is enabled for login, and the unit definition adds only the
bootstrap roots to the environment, never `PATH`; the user service manager
still passes its own environment block to the process, so the executable a
unit runs clears what it must not forward. Linux implements it with user unit
files `pohunek-<ns>-wf-<plugin-hash>-<j|s>-<name>.service`: a job is
`Type=oneshot`, bounded by `TimeoutStartSec=`, plus a `.timer` with
`OnActiveSec=` and `OnUnitInactiveSec=` (the next run one interval after the
previous one ended); a service is `Type=exec` with `Restart=always` and
`RestartSec=` waited after each exit. Both disable start rate limiting and
kill the whole control group. macOS implements it with LaunchAgents
`~/Library/LaunchAgents/io.github.zajca.pohunek.<ns>.wf.<plugin-hash>.<j|s>.<name>.plist`
in the `gui/<uid>` domain, written and replaced through the same crash-safe
registration as the daemon agent: a job has `StartInterval` set to the
interval, `RunAtLoad=false` (the first run one interval after load, then the
load-relative cadence), and `ThrottleInterval` of one second, because launchd
otherwise spawns a job at most every 10 s; a service has `KeepAlive=true`
and its restart delay as `ThrottleInterval`. launchd skips an interval firing
while a run is active and never runs two processes of one label, so runs
never overlap. A unit's process is matched by its executable, exact argv,
and the `XPC_SERVICE_NAME` entry launchd sets to the label, so two
installations running the same argv never act on each other's processes.
launchd has no key that bounds a running job and reports a job's exit
status only in `launchctl print` text, which the backend never parses, so on
macOS the executable a unit runs is the only run bound and the caller's own
run record is the only failure record; `ExitTimeOut` bounds a stop, after
which the backend kills the rest of the job's process group captured before
the stop. Stopping a unit boots it out but keeps its definition,
so it runs again at the next login until it is removed. Worker discovery
never matches these names or labels, so it neither adopts nor sweeps them.
`pohunek workflow enable` installs a plugin's job and service units through
this contract, and `disable` and `uninstall` remove them
(`crates/cli/src/workflow/units.rs`).

The shared contract crate compiles and tests natively on Apple Silicon Darwin
with a macOS 14 deployment target. macOS 14 is a supported build target without
CI acceptance; CI and release acceptance run on macOS 15 only (revision of
2026-10-16, #1224). Intel Macs are outside the current release scope. The secure runtime-path and portable filesystem contract also runs
natively on APFS, and native process and peer inspection are in place. The
session worker's PTY I/O is portable: output readiness is one `poll(2)`
implementation shared by both targets, and the worker's complete test suite,
including idle-CPU and descriptor-release evidence, runs on the native runner.
The daemon and CLI also build and test on that runner, including the
real-launchd supervision suite. macOS on Apple Silicon is a supported
platform. Release archives are ad-hoc signed and not notarized. The open items
of #100 and #102-#105 are ordinary follow-ups.

The host daemon is the local control plane for one machine, written in Rust.

Core responsibilities:

- Own logical session intent, metadata, durable transactions, and the public
  session lifecycle.
- Discover, validate, and reconcile per-session runtime workers before
  advertising readiness.
- Track session metadata, project records, worktree bindings, agent type, and
  runtime state.
- Store local metadata in one file-based store (JSON-lines today; an embedded
  SQLite store is a deferred optimization — see "Configuration, State, and Log
  Storage").
- Write structured logs and append session-lifecycle events to a local event log.
- Record immutable launch-native references for explicit recovery.
- Serve two listeners with one protocol:
  - a **Unix socket** for local clients;
  - a **TCP listener bound to the configured overlay's address** for remote
    clients.

The daemon must not depend on libghostty and never owns or receives a PTY file
descriptor. It proxies attach, input, resize, bounded screen/output observation,
inspect, and stop operations to the worker that owns the runtime. It also owns
the race-free `session.wait` resource model: snapshot, register, recheck, then
sleep without holding the registry write lock.

Daemon startup loads configuration and session metadata before accepting
commands. Missing or invalid required configuration fails startup with a clear
error rather than falling back to unsafe defaults.

### Concurrency and supervision

- The daemon uses Tokio. Each worker generation runs as its own native service
  job, so one worker failure cannot terminate the daemon or any other session.
  On Linux that is a systemd transient unit
  `pohunek-<ns>-worker-<session-id>-<generation>.service` started through
  `StartTransientUnit` (`Type=notify`, `KillMode=control-group`,
  `SendSIGHUP=yes`, `Restart=no`) in `pohunek-<ns>-sessions.slice`. On macOS it
  is a launchd job `io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`
  bootstrapped into `gui/<uid>` from a private definition directory, with no
  `KeepAlive`. `<ns>` is derived from the user ID and the canonical state and
  runtime roots.
- The daemon job (`pohunek-<ns>-daemon.service` or the launchd agent
  `io.github.zajca.pohunek.<ns>.daemon`) and worker jobs are siblings. Workers
  have no restart policy and no `PartOf`, `BindsTo`, or other stop-propagation
  dependency on the daemon. `pohunek service install` installs the daemon job;
  the daemon starts and retires worker jobs itself through one lifecycle engine.
- A stale public Unix socket is detected and replaced on daemon startup. The
  daemon holds independent lifetime locks for its durable state and data roots,
  plus a runtime-instance lock, so independently configured XDG roots cannot
  let two daemons control the same persistent records. Under systemd the daemon
  sends `READY=1` only after store load, worker
  discovery, reconciliation, and public socket bind.

## Durable Session Workers

Every live logical session has one opaque `worker_id` and one worker instance id
(public field `worker_instance_id`; `WorkerInstanceId` in the Rust crates). The
private worker control wire, data-frame headers, hook messages, the worker
journal and the persisted runtime record spell the worker instance id
`runtime_id`, so a live durable worker keeps talking to a newer daemon. The environment marker is
`POHUNEK_WORKER_INSTANCE_ID`, the only name the process inspectors, the sweep
and the hooks read. Workers of releases before 0.33.1 mark their children with
another name, so startup does not adopt such a live worker (`incompatible`,
`worker_marker_legacy`, judged from the journal's `version`; the upgrade
preflight reports the same code), and a sweep never signals a worker child
(`POHUNEK_WORKER_ID`, which only a worker sets) that carries the swept session's
`POHUNEK_SESSION_ID` but no worker-instance marker: it reports the process as
skipped and the cleanup stays unconfirmed. A lifecycle hook the daemon runs for
the session carries the session marker but no worker marker and is not part of
the sweep. The CLI flag is
`--worker-instance-id`.
The worker owns the PTY master, root child handle, reader and reaper, bounded raw
output ring, terminal tracker, input deduplication, resize sequencing, and final
outcome. Resize sources are scoped to the controller lease (`daemon-resize-<lease id>`),
so a resize ordering never spans a daemon restart: a replacement daemon numbers
its resizes from the start under its own lease and the worker applies them. The
daemon logs a warning when the dimensions a worker reports differ from the ones
requested. The daemon owns the stable session id, launch snapshot, project and
worktree association, native recovery reference, desired state, and lifecycle
transaction.

Disconnecting or killing `pohunekd` releases only the private controller lease.
The worker continues draining output and running the unchanged child. A
replacement daemon joins the service manager's worker jobs with worker journals
and sockets per worker generation, validates the job's process and start
identity, negotiates the private protocol, acquires the one-controller lease,
calls `Inspect`, and reconstructs detector and procwatch state. It does not
invoke native resume. A job whose worker is proven dead has its generation's
leftover processes swept by ownership marker before the session is reported
`lost`; unavailable or ambiguous supervisor evidence never kills anything.

The daemon stops its own background work in one place. The session registry
spawns every watcher, exit committer, forwarder and retry through a tracker it
owns, and runs its blocking store writes through the same tracker. At shutdown
the daemon first drains the creates still running, stops the tasks that drive the
registry from outside, and then calls `SessionRegistry::shutdown`. That refuses
new background tasks, cancels the per-session tokens, aborts the work that has no
cancellation of its own, and waits, bounded by `shutdown_timeout` (`deadlines.registry_shutdown_ms` in `service.toml`, optional, 5 s by default), until nothing
tracked is left; tasks still running at the deadline are named in the log. After
that the registry writes nothing: a store write through it is refused with
`registry_shut_down` and logged at ERROR with its location. The worker seals its
journal in the same spirit (the directory of a session is deleted only after
its record, or by the startup collection of record-less sessions whose workers
are gone and whose journals are older than `[worker_journals] max_age_days`;
see `docs/runbooks/durable-session-workers.md`): once the terminal record is committed, the only
replacement it accepts is the terminal acknowledgement (`terminal_acknowledged`
turning true). Any other write is refused and logged as
`worker.journal.sealed_write_refused`, because it would change an outcome the
daemon may already have read.

Each session also counts the input its watchers have been handed and not yet
applied: a rescan request, an output chunk of the detector, a worker snapshot or loss, a
delivered process exit, a due supervision retry and a scan or detector tick that
fired. The count rises where the input is handed over and falls after the watcher
applied it, store write and event emission included, so a producer cannot hand
input over without it being counted. Input that is only scheduled (a scan timer,
a retry delay) is not counted until it fires, and input still on its way from a
worker or the OS is not counted until the daemon receives it. Test builds expose
the count as `SessionRegistry::settled`.

The private worker protocol is local-only and owner-private. It uses bounded
newline-delimited JSON for control requests and binary-safe framed data
connections for output and attach traffic. A worker accepts one leased daemon
controller and one-use, short-lived data tokens. The daemon and worker support
the current and immediately preceding worker protocol versions; an incompatible
worker remains alive and is exposed as `runtime.state=incompatible`.
When a new attach capability is unavailable on the preceding version, the
daemon uses that version's bounded replay attach path. A daemon upgrade must
never make an otherwise live PTY unreachable merely because it cannot provide a
newer terminal snapshot feature.

A worker answers the OSC 7501 program-status probe for its own PTY when the
daemon sets `answers_program_status_probe` in `Initialize` (an additive field: a
worker that predates it ignores it and never answers). The daemon derives the
flag from the manifest of the resolved launch, the profile's override manifest
when there is one: it is true only when that manifest declares
`[program_status]` with `answer_probe = true`. A shell session, Hermes, Codex
(which never queries) and every manifest without the key behave exactly as
before, and `cat` of a file that holds a probe types nothing into a shell. The
reply is the fixed `ESC ] 7501 ; ? ESC \`, which carries no CR or LF. The reader
thread never writes: the output hub queues one coalescing reply token before it
fans a chunk out, and a dedicated task writes the reply through the write
coordinator, so it lands between input plans and attach frames and is dropped
after the control stall bound when the PTY does not read. The reply equals the
query, so an echoing program would loop: after a reply was queued, an
immediately following probe (no other output byte between) is not answered. The
reply is no input evidence: it arms no input token, submits nothing and never
touches the submit mark.

Retained replay and terminal repaint payloads are split into contiguous frames
bounded by the negotiated worker data payload. History capacity is independent
of the per-frame allocation limit: increasing retained history must never
require one proportionally large frame.

Private worker protocol v4 adds `ControlPlaneObservation` without changing
attach capabilities. A dedicated one-use data stream returns a runtime-bound
rendered terminal snapshot or a bounded retained-output replay, including
multi-frame responses, exact cursors, newest-tail reads, retained-history gaps,
and waits at the current end. Observation never acquires attach ownership or
changes terminal geometry. A v3 worker remains valid for existing lifecycle and
attach operations but reports observation unavailable; the daemon maps private
failures to stable payload-free public errors.

Private worker protocol v8 lets the controller forward a public agent report or
release into the worker (`RecordActiveIdentity`, `ReleaseActiveIdentity`). The
worker validates the subject like a hook claim does (hook schema, claim expiry,
PTY-tree membership and start identity of the process, ordering against every
earlier claim of that process), journals the claim and acknowledges only after
the journal write returns. The peer-side checks do not apply, because the peer is
the lease-authenticated daemon. A forwarded claim is marked `origin = forwarded`
and carries the daemon's opaque profile label and an optional session
annotation, but never a native reference: it cannot reach the launch identity,
the pending launch claims, the native reference claim or the resume binding. The
journal counts every change of the active identity or its release in
`active_identity_revision`, and the v8 snapshot reports the revision, label,
origin and annotation. A connection that negotiated v7 sees none of those fields,
so the claim reads as a provider-only active claim there and in an older reader
of the journal.

Private worker protocol v8 also carries prompt delivery evidence. The write
acknowledgement reports `submit_mark_ns`, the worker's `CLOCK_MONOTONIC` reading
taken just before the plan's last fragment is written, and the inspect snapshot
reports `prompt_accepted_sequence`, the last applied prompt report of the
runtime. A connection that negotiated v7 sees neither field, and the daemon then
never matches an acceptance to a submission.

Two properties of the private worker protocol carry the daemon's proof that a
forwarded request was rejected, so a change to either needs the settlement rule
below revisited. First, the worker handles the requests of one control
connection strictly in the order they were written and writes each response
before it reads the next request. Second, the worker grants its controller lease
under a fresh lease id and refuses another controller until the holder's
connection ended, which is after the request it was handling finished.

The inspect snapshot also reports `drains_streams_on_terminal_ack` (absent means
false): a worker that sets it lets attached data streams finish before it
answers `AcknowledgeTerminal`, and the daemon acknowledges no worker that omits it.

The daemon forwards a bound `session.report_agent` or `session.release_agent`
(it carries `pid` and `monotonic_sequence`; the daemon reads the start identity of
the process when the caller sends none and confirms it when the caller does) once
the existing admission has passed. The forward and its commit run in a task the
daemon owns, so a hook that gave up on its answer cannot cancel a half-done
commit. A worker refusal is final and answers `recorded: false`; the one
exception is a release for an identity that was only ever held in memory, which
the memory release rule still clears. A release that cannot be forwarded never
ends a claim the worker journaled: the journal is the truth for it, and the exit
watch of the reporting process ends it. A memory-only report replaces a journaled
claim only from the same process (pid and start identity) with a newer public
sequence; a report that names a pid without its start identity is bound by the
identity the host reports for the pid. After an acceptance the daemon commits the
claim the worker holds through the projection of a worker snapshot. A per-session
identity-commit guard serializes every claim-changing public operation (check,
forward, commit) with every other snapshot projection: a report or release is
checked against the claim it meets and committed inside one critical section, and
the public order (source, label, sequence) that produced the claim is recorded
with it, release tombstone included. A claim that waits for its commit counts as
the held journaled claim, and a release that waits for its commit holds the
journal empty: a memory report is judged against that release only. A memory
claim or release recorded while an operation waits carries the newest revision
the worker accepted as its floor, so the adoption of that operation and every
later projection at or below it leave the memory claim alone; a revision above
it replaces it. A request that timed out before it was written leaves nothing pending. An operation whose answer timed out, or was lost after the request was sent, stays pending with an
unknown revision, and a memory claim recorded after it holds against the
snapshot whose change is that operation, at any revision. The daemon stamps each
request it completes writing on a worker connection with the position of its
connection among the leases it acquired and its position on that connection, and
keeps the newest snapshot that its periodic worker sync or the commit of a
forwarded operation read (applied or not) with the stamp of its request. A snapshot read after the worker handled a pending operation's request
that still shows the identity revision the operation was sent at proves the
worker did not journal it: the operation is dropped as rejected, and the journal
is judged by the claim it really holds. A snapshot read earlier, or one that
shows a newer revision, leaves the operation pending until its revision settles
it; a changed worker instance makes it obsolete. The worker call and the snapshot read that follow it are
bounded by the worker connect deadline (`service.toml` `worker_connect_ms`); a
timeout keeps the report memory-only or leaves the commit to the next sync. A fence of the newest committed identity
revision (per worker instance, in memory) makes a snapshot read before a commit
leave the active identity alone while its native reference, subagents and
background still apply. A report that cannot be forwarded takes the memory-only
path: it is unbound, the worker negotiated v7, or the forward failed in transit
or in the worker's journal. The last two keep today's `recorded: true` when the
daemon's own rules accept it, with a WARN for a failed forward, and an INFO
`session.active_identity.not_durable` per session and cause names each of them.
A forward the worker accepted whose snapshot the daemon could not read or apply
in time logs `session.active_identity.commit_deferred` at WARN: the worker
journaled the claim and the next sync commits it. A v7 worker reports no revision, so no fence applies to it.

Public observation is bounded by the 1 MiB control-line ceiling. Output accounts
for base64 expansion and response metadata; screen snapshots are measured after
JSON serialization. The default maximum wait is eight seconds. Waiting
`session.output` and every `session.wait` occupy dedicated public connections
and one daemon waiter slot, capped by default at 128 globally and 8 per session.
The timeout is the guaranteed release bound: disconnect is not promised as
immediate cancellation because one connection dispatches one request at a time.
Runtime generations, output offsets, terminal watermarks, process-start
identities, and report sequences are canonical unsigned decimal strings on the
public wire so JavaScript clients preserve exact values beyond 53 bits.

Control input uses a bounded exact recent-result map and a monotonic watermark
scoped to the active controller lease. A reconnect acquires a fresh lease and
can safely restart its sequence. Raw attach input instead uses the ordered
stream's monotonic sequence and never consumes lifetime dedup capacity. This
keeps retries conservative without allowing a probabilistic structure to reject
fresh interactive input.

## Transport and Control Protocol

Public protocol v4 currently exposes one logical owner protocol over two
transport classes:

- **Local:** Unix domain socket at `<runtime-root>/daemon.sock`. A valid explicit
  `XDG_RUNTIME_DIR` selects `$XDG_RUNTIME_DIR/pohunek` on Linux and macOS. Linux
  requires that variable; macOS without it uses
  `/private/tmp/pohunek-<effective-uid>` and never selects a root from `TMPDIR`.
  The application directory is mode `0700` and the socket is mode `0600`. The
  validated owner-private root and socket are the access-control boundary.
- **Remote:** one TCP listener per configured overlay, bound **only** to the
  provider's validated current local member address and per-overlay port, never
  `0.0.0.0`. Reachability and authentication are provided by that overlay;
  NetBird is the default provider and Tailscale a supported opt-in provider.

The control protocol is **newline-delimited JSON**: one JSON request per line,
one JSON response line for ordinary requests. Long-lived subscriptions keep the
connection open and stream event envelopes as newline-delimited JSON. Requests,
responses, errors, and events are typed Rust structs serialized with Serde.

Every control request carries an `id`; responses and related events echo it so
cross-host operations are traceable in both hosts' logs.

### Protocol versioning

Public protocol requests carry an explicit inclusive
`{minimum, maximum}` version range. The daemon selects the highest overlap in
the first response; that integer version remains fixed for every later response
or event on the connection. A genuinely incompatible pair fails with
`daemon/version_mismatch`. The legacy integer request envelope is rejected: v2
is a one-time coordinated pre-1.0 boundary with no compatibility shim. Once all
clients and local/remote daemons cross it, later peers can overlap on an older
common version instead of requiring exact maximum-version equality.
The bounded N-1 rule that governs those later overlaps is the
[upgrade window](#upgrade-window) below.

#### Protocol window and edge adapters

A daemon of release N negotiates `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`
(`SUPPORTED_PROTOCOL_VERSIONS`) with the minimum one below the maximum, so it
serves release N-1's public protocol next to its own (today `3..=4`). The Rust
client and the CLI advertise the same window: `CLIENT_PROTOCOL_VERSIONS` is
`SUPPORTED_PROTOCOL_VERSIONS`, so the window has one definition. A client of
release N against a daemon that only speaks N-1 (a staggered multi-host
upgrade over an overlay) therefore negotiates N-1 and translates through the same
adapter the daemon uses, in the opposite direction. The TypeScript SDK
advertises the same window (its constants are generated from the Rust ones) and
carries a port of the adapter in `sdk/ts/sdk/src/compat.ts`. The port is checked
against the same recorded fixtures and uses key names and the introduced-method
list generated from the Rust adapter, so `cargo xtask ts check` catches drift
([#527](https://github.com/zajca/pohunek/issues/527)).

Core types describe only the current wire shape. `crates/protocol/src/compat/`
holds one adapter module per older version in the window (`v3`). An adapter
translates by shape only: renames and moved fields at typed locations (free-form
values such as session metadata are never walked), plus withholding what the
older version cannot decode (events it never defined, enum values such as
warning kinds it never knew). The daemon applies it at two choke points:
`handle_request` upgrades the request parameters and downgrades the response,
and the subscription writer downgrades every event for its own connection,
because the negotiated version is fixed for the connection's life. Handlers and
the event bus only ever see the current shape.

The Rust client applies the inverse at its own choke points
(`crates/client/src/transport.rs`). Once its connection has selected N-1 it
downgrades each request (`compat::downgrade_request`, pinning the request to the
selected version) and upgrades every success payload and subscription event
(`compat::upgrade_result`, `compat::upgrade_event`), so callers, `--json` output
and `Subscription::next_line` only ever carry the current shape. A fresh
connection sends a request that means the same in both versions as it is and
learns the version from the first response; a request whose translation depends
on the version (it carries a worker identity, or the method is new in N) first
sends `daemon.health` on the connection. A dedicated wait/output connection
inherits the version its parent selected. A payload the adapter cannot
translate, or a daemon that sends the current spelling under N-1, is refused
with `daemon/version_translation_failed`. Discovery probes offer the asking
client's range and classify a peer as reachable when it answers inside it, so
`host list` and the multi-host fan-out include N-1 hosts for a client that can
speak to them. `daemon.health` and
`host.inspect` report the version negotiated for the asking connection, which is
what an N-1 SDK handshake compares with its own version.

Two rules keep the window bounded:

- **Semantic change rule.** An adapter cannot paper over a change of meaning: a
  new required parameter, a removed method or event, a changed error code or
  changed behavior behind an unchanged name. Such a change raises
  `MIN_PROTOCOL_VERSION` to `PROTOCOL_VERSION` in the same bump, no adapter is
  written for the version it strands, and the release notes announce the break.
  Additive changes (optional fields, new methods, new error codes) need no
  bump; an event or enum value new to the daemon is withheld from an older
  subscriber by the adapter's known-event and known-value lists.
- **New-in-N rule.** A method added after the previous release has no
  previous-release fixture and no older shape to translate to. It goes on the
  adapter's `INTRODUCED_METHODS` list (`crates/protocol/src/compat/v3.rs`), and a
  connection of the older version gets `method_not_found` for it; its results
  are never translated or sent. A client of release N refuses such a method
  before sending it to an N-1 daemon, with the typed
  `daemon/daemon_protocol_too_old` error that names the host, the daemon's
  protocol version and the required one. An event added after the previous release is
  withheld from older subscribers by the known-event list. The coverage tests
  stay strict: a method without a fixture that is not on the list fails them.
  The list is deleted with its adapter.
- **Adapter deletion rule.** A `PROTOCOL_VERSION` bump deletes the oldest
  adapter and its fixtures in the same change and adds the adapter for the
  version being left behind.

Golden fixtures for every method and event family live under
`crates/protocol/tests/fixtures/compat/v<N-1>/`; consumer recordings (CLI,
TypeScript SDK, Hermes plugin, managed hooks) live under
`crates/daemon/tests/fixtures/compat/v<N-1>/consumers/`. They are serialized or
recorded by the previous release's own code, never written by hand;
`generator/golden_v3.rs.txt` shows how to regenerate them from a checkout of the
previous tag. `crates/protocol/tests/compat_v3.rs` fails when a method or event
has no fixture, when a current type stops decoding a renamed fixture, or when the
adapter and an independent blind rename of the fixture disagree. The real-daemon
test `crates/daemon/tests/protocol_window.rs` replays the recordings and checks
the per-connection shapes.

The accepted relay path is a coordinated public protocol v4 cutover owned by
[#70](https://github.com/zajca/pohunek/issues/70). Inside userspace WireGuard,
the host opens a TCP control link and serves typed NDJSON requests from the
enrolled relay; separate host-initiated TCP streams carry attach bytes. The v4
connection origin is derived from the accepted listener/link and cannot be
selected in request JSON. There is no v4 relay-path compatibility shim. Direct
Unix and overlay owner contexts remain relay-independent.

### Upgrade window

The no-shim rule has one bounded exception. The contract is that release N
carries release N-1 in four places so that an update never strands live work
(the same contract is in [the project instructions](../.agents/instructions/project.md)). Each place below
states the contract and then the current state.

- **Live workers.** The private worker protocol negotiates the range
  `PREVIOUS_VERSION..=CURRENT_VERSION` (`crates/worker-protocol/src/version.rs`,
  currently 7 and 8), so a worker started by N-1 keeps serving through a daemon
  update. A worker below the window is not adopted, and the upgrade preflight
  reports its session as `would_not_be_adopted`.
- **Worker journal.** The daemon reads the previous journal schema. The
  readable schemas are the explicit list `WORKER_JOURNAL_READABLE_SCHEMAS`
  (`crates/daemon/src/runtime/lifecycle.rs`); a schema is listed only when its
  layout names the worker generation, because a generation is never inferred
  from the path, the persisted record or the supervisor job. Today only the
  current `WORKER_JOURNAL_SCHEMA_VERSION` (4) is listed; any other schema gets
  a typed reason and a WARN and is never used as generation evidence. The
  optional journal field `worker_spawn_id` (the worker's kernel creation number,
  decimal string) is additive and needs no schema bump; the journal of a worker
  started by the previous release lacks it, and the runtime sweep then has no
  fork-lineage proof for processes whose environment the kernel withholds. The
  optional field `worker_coalitions` (the worker's kernel resource and jetsam
  coalitions, `<resource>:<jetsam>` decimals) is additive in the same way; its
  absence leaves those processes decided by lineage alone. The optional field
  `worker_control_group` (the worker's unified cgroup path, valid only within the
  boot named by `boot_identity`) is additive too: its absence leaves the Linux
  sweep with start-time ordering alone.
- **Public-protocol clients.** The contract: a protocol change keeps the
  previous version served through adapters, so the advertised range is
  `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION` with a minimum below the maximum.
  An adapter translates shape only. A semantic change raises
  `MIN_PROTOCOL_VERSION` as well and is an announced break. The oldest adapter
  is deleted on the next bump. The current state: the daemon accepts `3..=4`
  through the `v3` adapter, and the Rust client and CLI advertise the same
  window and translate through that adapter against an N-1 daemon. A method new
  in N is refused client-side with `daemon/daemon_protocol_too_old`. The
  TypeScript SDK advertises the same window and translates through its port of
  the adapter, so no direction of the N versus N-1 pair needs a one-pass
  upgrade ([#527](https://github.com/zajca/pohunek/issues/527)). Mechanics, rules and
  fixtures are under "Protocol window and edge adapters" above.
- **Persisted daemon state.** Migrates from any older kept schema (see below).
  A session launched from a host profile by an N-1 that did not freeze the
  profile revision carries none, so after the upgrade it resumes only with `session resume
  --accept-profile-change`; a plain resume fails with `agent_profile_changed`,
  and the accepted relaunch freezes the current revision. A session of a built-in
  runtime that its official package adopts keeps its frozen revision when the
  profile differs only by the `package` and `digest` keys the migration writes,
  so it resumes without the flag.

Everything else stays unshimmed. A record that cannot be migrated or adopted is
logged at WARN, surfaced to the operator, and carries a recovery hint. A
`metadata.jsonl` line that no loader accepts stays in the file: every rewrite
re-emits it unchanged after the parsed records, so the line is never lost to a
later write; package gates treat it as possibly pinning any installed package.
The operator-facing gate of the window is the upgrade preflight. `pohunek
service check` and `pohunek service upgrade` of an existing service run
`pohunekd upgrade-preflight` of the new archive before any effect; it prints a
`PreflightReport` (`crates/service-config/src/preflight.rs`) with the dry-run
store migration (`crates/daemon/src/store/dry_run.rs`) and one verdict per live
session (`adoptable`, `would_lose_recovery`, `would_not_be_adopted`, each with a
stable reason code; `crates/daemon/src/session/reconcile/upgrade_preflight.rs`).
It decides with the same store loader, journal reader, resume-binding merge and
launch-identity check that startup reconciliation uses, from the store, the
worker journals and the process table. It takes no lock, writes nothing, stops
nothing and never connects to a worker socket, because the running previous
daemon holds each worker's single controller slot; the identity claims, subagent
state and protocol range a socket would give are judged from what the journal
recorded, through the same functions adoption uses (process-tree ancestry of the
launch and active identity processes included; an unreadable process table
fails closed). A worker that journals no hook schema is validated with the
schema its runtime declares, resolved by the function adoption uses from the
built-in definitions and the package store opened read-only (nothing is
created); only an unreadable runtime definition leaves it unavailable
(`worker_identity_unverified`). The remaining blind spots are the supervisor
job's identity and socket failures at connect time. A binding that needs the
runtime registry to complete its launch spec is repaired first by the function
startup uses, over the profiles and runtimes the new daemon will have, and is
reported as `native_recovery_unrepairable` only when that repair fails. A session whose launch pin names a built-in
runtime source of an earlier release is `adoptable` when the official package
that serves its runtime id is installed and adopts it, live or stopped. A session
whose launch pin names a runtime source the new binary does not serve, resolved
as resume resolves it against that binary's built-in runtimes and the installed
packages, and that no package adopts, is `would_lose_recovery` with
`runtime_source_not_served`, and so is any live session, resumable or not,
whose runtime resolves nowhere on the new binary, because every interaction
with it would be refused. A live worker journal without a store record is
listed as an unmanaged worker and does not refuse the upgrade, unless a store
line that names no session cannot be read (`worker_without_session_record`).
Sessions at risk refuse the upgrade with
`service_upgrade_sessions_at_risk` unless `--accept-runtime-loss`, the single
loss-accepting flag, is given or an earlier upgrade accepted the identical risk
(the CLI-owned ledger `service-accepted-risks.json`, matched against the
daemon-computed `acceptance_scope`; no session is changed); a store the new daemon would refuse
(`service_upgrade_store_unusable`) and a preflight that cannot run
(`service_upgrade_preflight_failed`) and a systemd user manager whose `PATH` cannot be read (`service_manager_environment_unavailable`) are never overridable. The report also carries the release's official runtime plan (`runtimes`, an `EnsureReport` of `ensure-official-runtimes --dry-run` judged against the registry as it will be after the plan is applied) and the stopped sessions that keep native resume or fork (`stopped_sessions`, judged by the same rules as live sessions and at risk under the same flag).  The official runtime step adds five codes, none overridden by `--accept-runtime-loss`: `service_upgrade_runtimes_refused` (the release's plan cannot be applied; nothing was stopped), `service_upgrade_runtimes_failed` (the release's runtime step did not produce a finished report), `service_runtime_backup_invalid` (a registry, catalog state or host profile cannot be backed up exactly), `service_staged_runtime_invalid` (the release's `runtime/` tree is not the layout the release assembles, or a sealed release lists a `runtime/` file it does not carry), and `service_upgrade_runtimes_unconfirmed` (the step's daemon run could not be ended and its exit confirmed, so the upgrade is kept and not rolled back). The preflight runs before any effect and again right
before the daemon is replaced (the previous daemon cannot be paused, and its own
adoption fails closed for what remains); a resumed upgrade whose daemon replacement
has not completed is judged again. A rerun for the active version and a resume
after the replacement skip it.

`scripts/release` diffs `STORE_SCHEMA_VERSION`, the protocol and worker
protocol constants, `WORKER_JOURNAL_SCHEMA_VERSION` and
`EXPECTED_INTEGRATION_VERSION` against the previous tag and requires a
release-notes line for each one that changed.

### Persisted state schemas

`metadata.jsonl` holds four record kinds: session, resume, worktree and project.
Each line carries `schema_version`; a line without it is schema 1. The current
value is `STORE_SCHEMA_VERSION` (`crates/daemon/src/store/schema.rs`, currently
11), and `SessionRecord.info` embeds the protocol type `SessionInfo`, so a
protocol field that reaches `info` is a persisted-shape change too.

Startup order in `crates/daemon/src/main.rs`:

1. `store::migrate_at_startup` classifies every line. A store at the current
   schema is left alone. An older store is migrated: each line is rewritten as
   raw JSON through the `MIGRATIONS` steps from its own schema up to the
   current one, so a store several releases behind upgrades in one start and
   unknown fields survive until a step maps them.
2. Before the first migrating write the whole store is copied to
   `metadata.jsonl.pre-schema-<old>` (`<old>` is the oldest schema found;
   owner-only, fsynced, never overwritten, so an interrupted migration reruns
   from the original bytes).
3. Only then does the session registry start, so reconciliation and worker
   adoption only ever see a store at the current schema. Every other store
   access refuses a store that is not at the current schema.

`Store::dry_run_migration` (`crates/daemon/src/store/dry_run.rs`) performs the
same classification and migration in memory only, with no lock, backup or
write, for the upgrade preflight (see "Upgrade window").

A pending legacy migration manifest (`pohunek migration preflight`) is
validated against the original store bytes: the importer accepts a store whose
SHA-256 equals the manifest fingerprint, or a store that is exactly the schema
migration of the `.pre-schema-<old>` backup whose SHA-256 equals it. A store
edited before or after the schema migration is still refused with
`migration_store_changed`.

A store written by a newer daemon, or older than every kept step, is refused
with a typed `StoreSchemaError`; the daemon exits at startup and the store is
not touched. An interrupted service upgrade never automatically downgrades over a store
the previous daemon cannot read. Its rollback first judges that reader, then
stops the new daemon and judges the store again before starting the previous
version. A refused rollback keeps the transaction; restore the
`.pre-schema-<old>` backup taken by the migration or keep the newer release
installed. There is no automatic reverse schema migration.

The guard test `crates/daemon/src/store/shape_guard.rs` fails when the
serialized field set of a record kind or the serialized value set of a persisted
enum differs from `crates/daemon/src/store/fixtures/shape/schema-<N>.txt` without
a schema bump, and when an older schema has no migration step. The sibling guard
`crates/daemon/src/host_state/records/shape_guard.rs` does the same for
`governance.json` and `identity.json` against their format versions.

## Attach Streaming

Raw terminal bytes are never multiplexed onto the newline-delimited JSON control
connection. Attach uses a **separate connection**:

1. On the control connection the client sends
   `attach { session_id, initial_dimensions? }`.
2. The daemon replies with a `stream_id` (and, for TCP, the port/token to dial).
3. The client opens a second connection, sends a small header identifying
   `stream_id`. The worker applies the initial dimensions when supplied, emits
   one complete ANSI repaint of the current screen, and atomically continues
   with live PTY output. The connection then remains a raw, bidirectional byte
   pipe: terminal output flows down, the client's keystrokes flow up.
4. `resize`, `detach`, and other control actions are sent on the **control**
   connection, referencing `session_id` / `stream_id`, so they work while
   attached without escaping the byte stream.

This keeps JSON as JSON and bytes as bytes, is trivial to debug, and maps cleanly
onto both the Unix socket and the overlay TCP transport. Multiple clients may
attach to one session; resize policy when sizes differ is defined by the daemon
(last attach wins, with explicit resize control available).

While a worker answers the 7501 probe, it is the only responder for its PTY: it
drops every complete `ESC ] 7501 ; ? ... (BEL | ESC \)` reply found inside one
attach input frame, keeping the rest of the frame in order. A reply split across
frames passes through, which is harmless to the probing programs (they swallow
an unexpected reply). The client's DA1 and every other reply are forwarded
unchanged, and the control-plane `session.input` is not filtered. When a held
paced input plan delays the worker's reply past the client's DA1, the program
reads "no 7501" for that process and falls back to its screen output.

If the private worker stream reports a typed failure, the daemon closes the raw
pipe and retains that sanitized failure in a short-lived bounded result mailbox.
The client consumes it once through `session.detach` and stops reconnecting that
deterministic failure. Raw PTY bytes therefore remain unframed without losing
the worker's root cause.

## PTY/TUI Agent Runtime

The runtime path is real PTY/TUI agent sessions. **Codex, Claude Code, and the
local interactive Hermes Agent backend are first-class**. Hermes support is
verified on version 0.20.0, accepts later 0.x releases by the package's
range policy, and deliberately excludes its remote, browser,
desktop, gateway, ACP, and other non-local backends: Pohunek must own the PTY,
process, worktree, and diff on the same host.

The daemon compiles only the shell runtime in. Codex, Claude Code and Hermes
are not compiled in: only the official `pohunek.runtime.codex`,
`pohunek.runtime.claude` and `pohunek.runtime.hermes` packages, installed from
the release catalog, serve `codex`, `claude` and `hermes`. On a host without
the package, a bare `codex`, `claude` or `hermes` is `agent_profile_not_found`,
a host profile over it that pins no package is `runtime_not_installed`, and
both hints name the catalog install (and, for the profile, `pohunek plugin
profile migrate`). A session launched from the built-in Codex, Claude Code or
Hermes of an earlier release is refused with `runtime_builtin_removed` while
nothing serves its runtime id. Once the official package serves that id, the
package adopts the session, live or stopped, when the session's launch pin
names that package id; the package version may differ. Resume and fork then run
through the package, and the relaunched run is pinned to the package digest. Resume
and fork additionally require that the package honors the session's frozen
recovery contract: the same native reference kind, a hook-reported (not
core-assigned) reference, and for fork, fork support in both the frozen spec and
the package. A Hermes fork still answers `agent_fork_unsupported`. A session
whose recovery contract the package cannot honor is refused with
`runtime_served_by_package`, and its recovery is a new session. So is a session
whose pin names no package (an unpinned record of an earlier release). A
foreground or unmanaged `codex`, `claude` or `hermes` process is attributed only
through the installed package's detection manifest (and, for Codex and Claude
Code, its `[transcripts]` table), so without the package it is not observed as
that agent.

Runtime responsibilities:

- Start one agent subprocess inside each worker-owned PTY.
- Preserve actual terminal interaction for attached clients.
- Allow clients and the daemon to disconnect without killing agents.
- Track `idle`, `working`, `blocked`, `done`, and `failed` states.
- Store a validated immutable native recovery reference without using it for
  normal daemon reconnection.
- Bind each session to host, project, repository, worktree, branch, agent type,
  logs, events, and resume metadata.

### Launch source and special-case guard

Launch, resume, fork, input framing, detection, probes, profile bases and
runtime inventory resolve an agent through the daemon's runtime registry; no
per-agent code path exists for them. A launch names its agent through a
`LaunchSource`. `OwnerLocal` carries the name the owner typed on this host (a
host profile, else an installed runtime id) and is what `session.new` uses
today. `RelayProfile` carries only the name of a locally approved owner
profile and the profile revision that was approved. It can never name a
runtime id, package, program or argv: a bare runtime id is answered with
`agent_profile_not_found`, and a profile edited after approval (its file, the
detection manifest it names, the launch binding of its base runtime, or its
effective program and arguments) is refused with
`agent_profile_revision_stale`. A profile revision is an HMAC-SHA256 under a
host-local secret (`profile-revision.key` in the owner-private host-state
directory, created on first use) over those inputs, so a different login shell
changes it, a revision never lets a holder confirm a guessed `[env]` value, and
it is stable across daemon restarts. A missing or unreadable key fails closed
with `agent_profile_revision_unavailable`; there is no unkeyed fallback. A
profile-backed `session.new` freezes the revision into the session's resume
binding, so it needs the key; a launch of a bare runtime never does.
`session.resume` and `session.fork` resolve the profile once, compare its
current revision with the frozen one and refuse an edited, legacy or missing
profile with `agent_profile_changed` or `agent_profile_missing` unless the
local owner accepts the change. The one exception is the migration pin: for an
adopted built-in session the comparison also accepts a profile whose revision,
computed without its `package` and `digest` keys, equals the frozen one, with
the stored built-in binding, the frozen program and arguments, and the profile's
manifest in the input. A legacy binding without a frozen revision is never
accepted this way. Connecting a relay-origin
`session.new` to `RelayProfile` depends on the locally approved `HostShare` of
[#82](https://github.com/zajca/pohunek/issues/82); until then the relay
variant is covered at the resolution seam only.

A runtime declares its config home (the directory its agent keeps settings,
hook registration and conversations in) in an optional `[config_home]` table: a
variable name and a home-relative default. One resolver
(`crates/daemon/src/agent/host/config_home.rs`, applied by
`crates/daemon/src/integration/homes.rs`) answers what a launch would give the
agent: the variable read from the launch environment (the daemon's base
environment overridden by the host profile's `[env]`), else the default below
that environment's `HOME`. A non-absolute value is a typed error and is never
expanded with the daemon's `HOME`. An optional `subdir` joins the variable's
value only (never `default`), and two runtimes are ambiguous owners of a home by
its resolved path. The integration handlers take the resolved
directory instead of resolving one themselves, so install, status, doctor,
removal and a launched agent agree; `profile` and `all_profiles` on the
integration methods select one profile's home or every distinct home (one
transaction per canonical directory) and are served on the local control socket
only. A home that several runtimes reach through one handler (a package that
declares the Claude handler and Claude's config home, say) is attributed to
none of them when a request names no runtime: each reports it as the error
`integration_home_ambiguous_owner`, so neither provenance nor id order lets one
runtime act on another's home. The one home a handler keeps itself is that of a
built-in runtime this build does not compile in (the Codex handler keeps
`CODEX_HOME`, else `~/.codex`, and the Claude handler `CLAUDE_CONFIG_DIR`, else
`~/.claude`, of the 0.34 built-ins): status, doctor and uninstall that name such an
official alias while nothing serves it address that home, so the hooks it
installed can be reported and removed without a package; install never does.
Host profiles over that alias do not resolve then, so their homes are found
from each profile file's `base` and `[env]` alone, read under the same
containment and owner checks as a profile load.

The remaining per-agent names are data or work owned by other issues. A
source-scan test (`crates/xtask/tests/no_special_dispatch.rs`) pins every
production line under `crates/daemon/src` that still names `codex`, `claude`
or `hermes`, per file, with its owner (`handlers` for the integration handlers,
#68 for the profile doctor) or `data`. The external observer takes each transcript root and its
parser from the runtime's `[transcripts]` table
(`crates/daemon/src/agent/host/transcripts.rs`), so it names no agent. The pin
is the exact trimmed text of each naming line (a SHA-256 over them for the
large integration files), so a new, replaced or edited line and a stale entry
all fail the test.

### Agent state detection

This was validated against the source of `herdr` (same Rust + portable-pty +
Tokio stack), `Kandev`, and the provider hook contracts. The parent agent's
coarse activity remains terminal-derived; provider hooks are not treated as a
replacement for PTY evidence. State is derived from the terminal stream, in
priority order:

1. **OSC title / progress (primary).** Agents emit OSC 0/2 (title) and OSC 9
   (progress) for their own UIs. A working agent shows a spinner (Braille range
   U+2800–U+28FF) in the title; idle shows a non-empty title without a spinner;
   Codex signals blocked via "Action Required" in the title. OSC is parsed
   incrementally (sequences fragment across reads) and cleared on
   foreground-process change.
2. **Screen-content pattern matching (fallback).** The daemon runs a virtual
   terminal emulator over the PTY and matches the visible screen tail against
   per-agent rule sets (regions + contains/regex/any/not gates). This is how
   interactive approval prompts are detected (e.g. Claude's "enter to select" /
   "esc to cancel" form).
3. **PTY activity (working authority).** Bytes flowing = working. Idle
   transitions are debounced behind a stability window (recheck ~100 ms, ~3
   confirmations, ~700 ms cap) and gated on visible UI confirmation to avoid
   flicker.
4. **Process state.** Running vs exited, plus exit code for `done` / `failed`.

**OSC 7501 program status (optional layer above the title).** A runtime whose
detection manifest declares `[program_status] app = "<name>"` opts in to the
program-status protocol: the program reports its own state as
`ESC ] 7501 ; state=<working|blocked|done|idle|error|clear>[:app=..][:kind=..] ST`
when it changes. This is terminal-stream evidence like a title, never a hook, and
the screen rules stay the fallback.

- **Parsing.** The detector holds the latest *root* record whose `app` equals the
  manifest's. A record of a child (an `id=` pair, including a child `clear`), a
  record of another app and a record without an app are ignored; a root `clear`
  drops the held record. A malformed report is discarded whole: an unknown or
  missing `state`, a key over 16 bytes, a payload over 4096 bytes, invalid base64
  in `msg` or `title`, a decoded control character, or text over its decoded limit
  (2048 bytes for `msg`, 192 for `title`). A malformed pair is skipped and an
  unknown key ignored. `msg`, `title` and `progress` are validated and dropped:
  they are never stored, logged or exposed. `kind` counts only with `blocked`. A
  probe (`7501;?`) is never a record.
- **Arbitration.** One fixed rule combines the held record with the screen
  winner of today's evaluation, in `feed`, in `tick` and at every working-latch
  point. The record is always included, whatever the chunk's freshness: it is a
  state, not an event.

  | held record | screen winner | result | source |
  |---|---|---|---|
  | none, or no `[program_status]` | any | the screen winner, as without the layer | as without |
  | `blocked` (any kind) | any | `blocked`, a visible blocker | `osc_progress` |
  | `working`, `done`, `idle`, `error` | a visible-blocker rule | that screen `blocked` | `screen` |
  | `working`, `done`, `idle`, `error` | a rule with `overrides_program_status = true` | that screen result | `screen` |
  | `working` | anything else | `working` | `osc_progress` |
  | `done`, `idle`, `error` | anything else | `idle` | `osc_progress` |

  A visible dialog therefore always beats a non-blocked record, so a forged or
  stale record cannot hide a dialog the screen rules see. A held `working`
  cannot be undone by a screen-only chunk, and a held `idle` beats a stale
  working screen match. `done`, `idle` and `error` are one pohunek activity, so a
  restated or changed one among them is no transition (`error` folds into idle).
  Because a held record is structured evidence, the bytes-flowing fallback does
  not fire under it.
- **Lifetime.** A record carries no expiry. It ends with a root `clear`, with any
  detector reconfigure (an agent switch, a nested agent's exit, a report, a
  package refresh), with a lag resync, and with the runtime generation (a new
  detector holds none). It is not persisted and not in `SessionInfo`. After an
  adoption it is restored only when the retained output still covers it.
- **Readiness.** Initial-input readiness and the idle barrier ignore the record:
  they evaluate the screen winner alone, so a record never releases or holds a
  prompt.
- **Evidence.** A transition derived from a record takes the detector path of the
  input-evidence rules: `working` opens a turn only by taking an armed input
  token, and a record never arms one. A held record restated on later chunks
  changes nothing.
- **Wire.** The source is the existing `osc_progress` ("an OSC status report of
  the program"), so older SDKs that validate the closed `StateSource` set keep
  working. `session.detection` does not expose the record: the
  `DetectionRegionKind` enum is strict on the wire, so the `program_status`
  preview waits for the tolerant decode of #1337.
- **Probe answer.** Programs such as Claude Code and Pi enable their reports only
  after the terminal echoes the probe. The session worker answers it when the
  launch's manifest sets `answer_probe = true` (see Durable Session Workers).

Like forged titles and screen text, a forged record can set working or idle in a
session whose screen shows no dialog the rules see, because the PTY tree is not
an identity boundary ("Runtime Packages and Trust"). This is a documented limit,
not a defect.

States tracked: at least `working`, `blocked`/`waiting_approval`, `idle`, `done`,
`failed`, each carrying a `source` field for signal strength. Detection rules are
**data (TOML manifests)** so agents can be added or fixed without recompiling. The
`blocked` signal is the trickiest and is validated per agent and agent-CLI
version.

Hooks have separate roles:

- **Native recovery binding.** A launch-agent `SessionStart` hook prefers the
  worker socket and posts the agent's session ID or transcript path. The worker
  validates process ancestry and accepts this binding only from the launch
  process the session's hook schema declares; the worker alone applies that
  rule. Under `NamedForProvider`, the rule of the subagent-capable schema (and
  the core default for a session without a schema, never reached because such
  a session admits no hook report), that is the designated immutable launch agent (the earliest process at or below the
  PTY root whose executable is named for the provider, or is a release image
  `<provider>/versions/<release>` of its native installer) or the provider's
  hook-helper process the launch agent started directly (for Codex the
  `app-server` child it runs its hooks from; a same-provider child in any other
  role is refused). Under `FrozenRootEntrypoint`, the rule of the identity-only
  schema, it is the PTY root alone, reporting in process: the kernel peer of
  the report must be the root's PID and start time, and the root qualifies only
  while its command line and executable match the entrypoint the worker froze
  before spawning it (a native executable, or a script together with the
  absolute, argument-free shebang interpreter that runs it) and those files keep
  their frozen inode, size, modification and status-change times. Every other
  process is refused, including a descendant that names the root or is named for
  the provider; every later native reference of the root is verified the same
  way before it is journaled; and an entrypoint that cannot be frozen accepts no
  claim. The worker journals the accepted value before
  forwarding it to the daemon. This keeps recovery tied to the original launch identity and
  retains the claim across daemon outage. A notification hook uses the same
  endpoint: the worker attests the caller, binds the notification to its own
  session, and forwards it to the daemon's public socket; when the worker
  refuses or lacks the request type the hook dials the daemon directly. Every private report is additionally
  bound to its reporter: the worker accepts it only from the process it names or
  from a descendant of that process (under `FrozenRootEntrypoint`, a report
  naming the PTY root only from the root itself), using kernel peer identity the caller
  cannot supply. That is the shape the shipped hooks already have, and it stops
  an unrelated command in the same terminal from rewriting another process's
  identity. If the private endpoint cannot be reached, the hook uses the local
  public daemon as a hardened fallback. That path admits a report only from the
  kernel peer of its local connection when that peer is the session's PTY root
  itself, refuses every report for an `identity-v1` session
  (`native_report_unattested`), and then applies its runtime, ordering, expiry,
  and provider rules. The shipped Claude and Codex hooks are descendants of the
  agent, so their fallback is refused and their native reference arrives only
  through the worker. That
  report carries the exact runtime id, PID plus kernel start identity, a
  monotonic sequence, and a short RFC 3339 expiry. The daemon rejects stale
  runtime/session/provider identity, PID reuse, expired claims, and duplicate or
  out-of-order sequence values. Native identifiers remain redacted.
- **Nested active-agent reporting.** Shell sessions inherit the pohunek hook
  environment, so Codex or Claude Code started inside a shell PTY can report its
  active runtime identity through the worker. This sets
  `active_agent`, `active_agent_base`, and optional active native metadata, but
  it does not change the shell session's launch `agent` / `agent_base` and does
  not overwrite `native_session_id` / `native_session_path`.
- **Provider-managed subagent lifecycle.** Current Claude and Codex
  `SubagentStart` / `SubagentStop` hooks send only the provider, subagent id,
  optional parent id, and optional agent type to the owner-private worker. The
  worker validates the runtime and reporting process, rejects stale per-child
  sequences, journals concurrent subagents, and retains bounded terminal
  history. At every Claude main-agent `Stop` the Claude state hook also sends
  a `subagent_snapshot` of Claude's running background tasks (only each task's
  `id`, `type` and `status`); the worker completes only the running rows of
  the same reporting process whose end the snapshot proves, never a row with a
  newer claim, never reopens or re-sequences a row, and logs the reason only in
  the worker log. An unlisted row is completed when the reporter's newest
  listing of it falls within its run and is older than this snapshot; any
  other unlisted row is completed only when no listed task can host an agent
  and the reporter's newest snapshot is older than this one and newer than
  the row, because Claude builds the listing before the hook reads its clock.
  Only that newest evidence is kept, so a snapshot older than it, delivered
  out of order, settles nothing from it. A completed row keeps its own claim sequence. The state hooks validate their action and
  Pohunek handshake before
  reading, reject oversized JSON through a bounded direct pipe, and never stage
  provider payloads on disk. Prompt text, results, transcript paths, and raw
  hook payloads never cross this boundary. A state hook that fails to deliver
  a report still exits 0 and appends one payload-free line per reason to the
  owner-private, size-bounded `pohunek-hooks.jsonl` in the log directory
  (path and bound from the worker-injected `POHUNEK_HOOK_TRACE_PATH` and
  `POHUNEK_HOOK_TRACE_MAX_BYTES`; see "Hook failure trace" in
  `docs/public-api.md`). A report refused only because its session's worker is
  unreachable gets a retryable error after a short configured admission wait
  (`hook_admission_wait`, below the reporter's socket timeout) when it opts in,
  and the reporters retry it within their hook timeout. Running children become `lost` when
  their PTY runtime terminates. This state is independent of the parent
  session's coarse `activity` and survives daemon and client reconnects.
- **Prompt delivery evidence (Claude only).** The Claude state hook registered
  for `UserPromptSubmit` sends the worker a `prompt_accepted` report for each
  submitted prompt. The hook drains its input without decoding or keeping it,
  prints nothing, exits 0 on every path and makes one worker attempt with no
  daemon fallback; the report carries the provider, the reporting process and
  the hook's own `CLOCK_MONOTONIC` start time. The worker accepts it only from
  the process its journal names as the session's launch identity, so a nested
  `claude -p`, another provider and a shell-hosted Claude are refused, and only
  with a sequence ahead of the last applied one. It keeps that sequence in
  memory, and the daemon reads it from the periodic inspect snapshot. It is
  delivery evidence for a waited input, never activity: it moves no activity
  revision, emits no `agent_state` event, opens or closes no turn, and never
  arms the input evidence of a turn.

Live state remains detector-first: OSC (titles, progress and the opt-in OSC 7501
program-status records, terminal-stream evidence and never a hook), screen, PTY
activity, and process state continue to drive normal activity transitions. Notification hooks still target
the daemon and may be missed during daemon outage. A nested active-agent report is
explicit hook evidence and uses the `report` state source when it supplies
activity. While such a report is current, the detector can temporarily switch to
the active agent's manifest so Codex/Claude UI patterns are interpreted
correctly inside the shell session. Releasing the active report clears the
active fields and restores the shell/default detector manifest. See the resume
model below.
Under an agent manifest the detector also latches a working sighting inside one
output chunk: it evaluates the manifest at each completed OSC title or progress
event and at each ordered screen observation point, with the freshness of the
bytes consumed so far, and a `Working` winner that a later point of the same
chunk hides is published before the chunk-end evidence, but only when that
chunk-end evidence is idle: a chunk that ends on a dialog or on a screen no rule
names reports its own state alone, so transcript text drawn ahead of a dialog
never reads as working. The latch is a local of
one detector feed: it is not persisted, it closes while `Working` is already
published and after the first sighting, and it reaches the registry only as the
ordinary activity transition. The generic shell manifest never latches because
its working rules are loose substring matches. An agent manifest keeps the
ordered screen observer alive after startup so the latch sees every byte; a
manifest switched in under a shell resumes it. A stale spinner line left visible
under a dialog that a chunk erases before it redraws the idle prompt can latch;
this happens only in a turn that was already running. Bytes skipped on a
broadcast lag cannot be latched.
Each detector task is scoped to the exact worker id, runtime id, and runtime
generation that created its output stream. Cancellation is advisory; a final
transition from a superseded detector is discarded instead of being stamped
with a replacement runtime's identity.

Each session's procwatch loop scans its process tree on a blocking thread, never
on the executor that serves requests. It scans every `procwatch_poll` (1 s). A
session that is not working, holds no unbound hook claim and whose process tree
is unchanged between scans doubles the wait up to `procwatch_idle_poll_max`
(5 s); a changed tree, a `Working` transition, a hook or an exit watch returns it
to the base poll, so a new child of a quiet session is observed within that
ceiling.

Procwatch reads Linux `/proc/<root>/stat` `tpgid` as the terminal foreground
identity. A recognized process-group leader wins; an unrecognized wrapper lets
the oldest deterministic recognized member of that exact PGID win. A known
foreground group suppresses first-seen descendant fallback, so returning to a
shell cannot re-claim a background agent on the next poll. A recent unbound
hook claim remains valid until its claim TTL because foreground identity alone
cannot identify its process; process-bound nested claims clear immediately,
except a reporter that one scan proves alive below an observed agent of the same
runtime (or below an agent launch root): the foreground rules above then keep it
and bind its own start identity, and any scan, probe or notification without
that proof decides as if the exception did not exist.
The PTY-root claim is preserved only for a direct-launch agent matching the
launch provider.
Transient probe failures retain the last-known PGID and active claim. Provider
replacement clears report-derived activity and nested native metadata, switches
the detector manifest, and uses PID plus kernel start identity to reject reused
PIDs and delayed exit notifications.

### Agent input injection (TUI quirks)

Sending a prompt into an agent's PTY is not simply "write bytes + `\r`". From
Kandev's hard-won handling:

- **Claude Code (Ink TUI):** disable bracketed paste and send the submit byte
  (`\r`) as a **separate write after a ~150 ms delay**, or Ink's paste-burst
  detection absorbs the Enter into the pasted text and the prompt never submits.
- **Codex:** wrap multi-line prompts in bracketed paste (`ESC[200~` …
  `ESC[201~`) so embedded newlines are not treated as premature Enter; send
  the submit byte (`\r`) as a separate write after a short delay so newer TUIs
  do not absorb Enter into paste handling.
- **Hermes Agent 0.20.0:** the Hermes package uses bracketed paste and sends
  submit as a separate `\r` write after 150 ms. Every release that ships the
  package needs its compatibility gate to hold reviewed real-PTY evidence for
  the classic interface and either a capture or a recognized
  local-unavailable diagnosis for the alternate-screen TUI. Programmatic input
  accepts LF and tab but rejects other terminal controls
  (`text_policy = "hermes_safe_text"`); it is refused while a visible
  owner-approval prompt is blocked.
- **Other agents:** use the framing their runtime package declares.

A runtime package declares its input framing in the `[input]` table of its
descriptor `runtime.toml`, next to its launch command, detection manifest and
resume command: `bracketed_paste`, `submit_delay_ms`, `text_policy`
(`unrestricted` or `hermes_safe_text`), the optional
`submit_delay_configurable`, which lets a submit delay configured on the daemon
for that runtime replace the declared one, the optional `ready`
(`bracketed_paste`, `screen_idle` or `manifest_idle`) that says which signal
shows the agent reads initial input (for example
`runtime-packages/claude/runtime.toml`), and
the optional `startup_grace_ms`, the shortest wait for that signal the runtime
needs, which a longer host grace extends (for example
`runtime-packages/hermes/runtime.toml`). The
daemon parses the table into the runtime definition
(`crates/daemon/src/agent/host/definition.rs`), and the worker input plan below
applies it.

Every worker input plan preserves provider framing as two fragments: the body
fragment owns the provider submit delay and a separate fragment carries the
submit byte. The per-session input gate serializes that complete plan against
both waited and fire-and-forget calls. Fire-and-forget retains each provider's
blocked-input policy and delayed submit behavior.

An optional bounded input wait uses one overall deadline measured before
delivery. Waiting for the per-session input gate consumes the same budget
advertised to Rust and TypeScript transports. A waited request rejects blocked
activity with `session_agent_blocked` both before the gate and at the causal
boundary, independently of the provider's fire-and-forget policy. Nonzero-delay
wait framing is daemon-paced: the daemon writes the body as one worker plan,
waits out the delay itself, re-checks activity and runtime identity under the
session lock and writes Enter as a second plan, holding the input gate and the
worker control connection across both. Body text already consumed by an
arbitrary TUI cannot be retracted, so a blocked agent, an elapsed deadline or a
shutdown before Enter fails with `session_input_not_submitted` and no Enter. A deadline not longer than the delay is refused before the body is written. An
`idle` target on such framing counts only after `working` evidence above the
boundary captured before Enter's send.
For zero-delay framing, the daemon first obtains the exclusive worker write
reservation, then captures the runtime-scoped activity boundary immediately
before starting the prepared two-fragment worker plan. A timeout or shutdown
while acquiring that reservation cancels the unsent plan and writes no bytes.
Once send starts, the worker exchange remains cancellation-shielded and retains
the per-session input gate until its late acknowledgement is consumed. This fail-closed tradeoff
preserves provider paste-burst behavior without risking an Enter against a newly
visible approval UI. Once the atomic plan was sent,
delivery outcome may be unknown, so clients inspect the session and never retry
blindly. A fixed upper deadline rejects evidence observed later, including the
timeout recheck.
Each event carries its runtime identity, daemon `activity_epoch`, and exact
decimal-string activity revision. The registry retains the latest evidence for
the full maximum wait window, subject to a hard per-session memory bound, so a
later report of the same activity cannot erase valid pre-deadline evidence. The
dedupe cursor is `(activity_epoch, runtime, revision)` because a replacement
daemon adopts the same runtime but starts a fresh revision sequence. Runtime
exit returns `session_not_running`; replacement returns
`session_runtime_changed`. Rust and TypeScript SDK wait helpers reject a success
response unless it includes the matching activity, source, runtime identity,
activity epoch, and activity revision, preventing a same-version daemon that
ignored the additive `wait` request from confirming delivery.

A caller that opts in with `wait.allow_prompt_accepted` lets prompt delivery
evidence answer a `working` target when no `working` activity was observed.
The worker reads `CLOCK_MONOTONIC` just before it writes the last fragment of
the plan that carries Enter and returns it in the write acknowledgement
(`submit_mark_ns`, worker protocol 8 and newer); the hook reads the same clock
when it starts. A prompt acceptance answers the wait only when its sequence is
above that mark, it names the submission's runtime, the daemon received it
before the deadline, and the agent was not `working` at the boundary: a busy
agent queues the input, and its own `working` evidence answers. The result then
carries `prompt_accepted: true` and no activity, source or revision, because
no activity was observed. An opted-in result of a Claude session on worker protocol 8 or later whose hook
schema admits the prompt action states `prompt_accepted` whatever answered it;
any other session (another runtime, an older worker) gets no submit mark, no
field and the generic timeout message. An acceptance never answers an `idle` target and never stands in
for the `working` evidence delayed-submit framing needs first: the daemon
imports it up to one 100 ms poll after the agent read the prompt, before the
agent drew a spinner, so an `idle` restatement in that gap would read as a
finished turn. When neither kind of evidence arrives the wait ends at its
deadline with `session_input_timeout`, and for an opted-in `working` wait the
message names both. `session new --input-ack-timeout-ms` counts an acceptance
as the acknowledgement. A turn the agent starts on its own between the Enter
write and the agent reading Enter makes the wait succeed on that report; the
typed prompt is then queued, so an acceptance means submitted or queued. The
hook proves only that Claude started its `UserPromptSubmit` hooks: hooks of one
event run in parallel, so another user hook that blocks the prompt can discard
it after the report. Callers that need the work to have started wait for
`working` activity.

### Hermes operator plugin

The local Hermes Agent runtime (verified on `0.20.0`, accepted up to the next
major release) may load a Pohunek-owned plugin
from one explicitly selected Hermes profile. This is a CLI/client-side
integration; the daemon stays provider-neutral and M3 does not increase the
public protocol version. The plugin assets and the deterministically generated
`pohunek:pohunek` skill come only from the installed, selected official Hermes
runtime package (`runtime-packages/hermes`); the `pohunek` binary carries just
the core hook reporter and refuses to install or update the plugin without the
package (`hermes_package_not_installed`). Installation uses
only Hermes's supported plugin commands and places a separate owner-private
Pohunek policy outside the immutable plugin checksum set. The rendered plugin
asset stores the exact absolute policy path selected by the installer. Install,
update, setup and doctor accept the `hermes` executable only when
`hermes --version` names a release in the range the selected package's
`version_probe` declares, read with the grammar the daemon's launch gate uses
(`crates/package/src/version_probe.rs`), so the CLI never accepts a release a
Hermes launch refuses.

The policy fixes the verified absolute CLI executable, public protocol range,
access mode, host allowlist, and bounded tool limits. It is a delegated-tool
guardrail, not a same-user sandbox: a process with shell or file-write authority
can still bypass or alter same-user files. The plugin accepts no raw argv,
arbitrary daemon method, arbitrary endpoint, raw attach stream, force flag, or
environment map. It invokes a single fixed-argv CLI JSON runner and passes
untrusted prompt/input text only on standard input. Remote requests remain
direct connections to the selected host daemon over its overlay; no plugin listener,
proxy, SSH bridge, or central service is introduced.

Read tools expose hosts, sessions, inspect, rendered screen, bounded output,
wait, and diff. `manage` additionally exposes structured project/worktree/agent
profile session operations with exact unique-name resolution. `full` alone
registers stop and remove. The plugin repeats the daemon's origin-session guard
before subprocess start: it denies exactly `session.stop`, `session.resume`,
`session.remove`, `session.fork`, `session.resize`, `session.set_metadata`,
`session.rename`, and `session.input` when they target the hosting session. The plugin exposes no consent removal, so it has no counterpart for the daemon's denial of `session.remove_accepting_unconfirmed`. The
only origin-session exceptions are the lifecycle reports
`session.report_agent`, `session.release_agent`, and
`session.report_native_id`; the daemon remains authoritative.

Hooks run only in a Pohunek-managed Hermes process. They use a short bounded
local Unix-socket attempt, prefer worker-private identity reporting, and fall
back to the local public native-ID method, which accepts a report only from the
session's PTY root (see the attestation rule in the public API); the Hermes
reporter runs in the agent process, but its `identity-v1` session refuses that
method for every report, so the worker socket records the reference. They never start a
subprocess, access the network or Hermes `state.db`, emit terminal output, or
copy prompt/tool/output payloads. A failed hook is counted and swallowed, so
process and screen detection remain the daemon's fallback. `on_session_end` is
not a process-exit signal. A higher-sequence continuation identity reported by
`pre_llm_call` supersedes the launch identity for a later native resume.

## Runtime Packages and Trust

A runtime package is a content-addressed `tar.zst` archive with a runtime
descriptor (`runtime.toml`), a detection manifest and other checked data. It
carries no code that Pohunek loads. The archive format, the owner-private
registry and the signed catalog are in `crates/package`; the daemon turns a
verified package into a validated runtime definition
(`crates/daemon/src/agent/host/package.rs`) and runs install, enable, select
and uninstall in `crates/daemon/src/session/package_lifecycle/`.

| Class | Authorized by | Status |
|---|---|---|
| Official package | A signed release catalog entry that binds package id, runtime id, version and archive digest, verified against `runtime-catalog-anchor.json` beside `pohunekd` (`crates/package/src/catalog.rs`, `crates/daemon/src/catalog_anchor.rs`, `package_lifecycle/trust.rs`). | Shipped. Each Linux daemon archive carries the Pi, Codex, Claude Code and Hermes packages and the catalog under `runtime/`. Only an official package may serve the reserved ids `codex`, `claude` and `hermes` (`crates/daemon/src/agent/host/claim.rs`); `shell` is never claimable. |
| Third-party package | The archive digest the owner supplies (`--sha256`). `plugin link` copies a developer directory and installs it disabled. | Shipped. Never serves a reserved id. |
| Workflow plugin | The archive digest the owner supplies (`pohunek workflow install --sha256`). Not a runtime package: the archive carries `workflow.toml` and never `runtime.toml`. | Shipped ([#148](https://github.com/zajca/pohunek/issues/148)): the CLI-owned store `<state>/pohunek/workflow-plugins/` (a package registry of its own, never read by the daemon), the strict manifest, the consent install, and `pohunek workflow run`, which runs one action without a shell under the owner limits of `<config>/pohunek/workflow.toml` with a minimal environment that keeps the complete session origin pair, and logs a content-free record per run (`crates/cli/src/workflow/`, `crates/cli/src/commands/workflow.rs`). `workflow run --json` places the action's single JSON document byte for byte in the CLI envelope. Declared jobs and services run as user service manager units of the `pohunek service` installation that `workflow enable` installs and `disable`/`uninstall` remove; each unit runs the CLI's hidden `pohunek workflow __exec` trampoline, which owns the overlap lock, the wall-clock job schedule, the service backoff and the stop handling (`crates/cli/src/workflow/units.rs`, `crates/cli/src/workflow/supervised.rs`). Shipped: event reactions. A plugin's `[reactions.<name>]` tables run its argv once per matching public event (the protocol's event table minus `attach_opened` and `attach_closed`). One dispatcher unit per namespace, running the CLI's hidden `pohunek workflow __dispatch`, reads the daemon's `subscribe` stream as a separate process: it never starts the daemon, reconnects with bounded backoff, drops events for a full per-plugin queue with one `dropped` record per overflow episode, and never delays the daemon or another plugin (`crates/cli/src/workflow/dispatch.rs`). A plugin is an external command that runs as the owner and calls the public CLI or SDK. The v1 boundary ([#480](https://github.com/zajca/pohunek/issues/480)) gives core no channel or record for plugin data and no launch-time policy hook; a launch still names one concrete, locally approved profile. |
| Daemon, overlay, session worker or GUI extension | None. | Unsupported. Protocol methods, overlay providers, worker behavior and GUI code stay compiled and audited in core or in the client that owns them. |

An integration handler may also contribute per-launch input
(`crates/daemon/src/integration/launch_input.rs`). The handler's contributor
returns pure data for one launch: environment pairs, root-position arguments
and private files. Core validates it, writes the files atomically as
`launch-input.<name>` (owner-only) in the session's worker state directory, which
neither the agent nor its sandbox can write and which is deleted with the
session's journals, and resolves `{file:<name>}` placeholders to their absolute
paths. The contribution is rebuilt for every create, resume and fork and applied
in one place per launch: the environment between the profile `[env]` and the
daemon handshake, the arguments after the frozen arguments and before the native
session arguments. It is never part of the frozen resume snapshot, so a relaunch
applies it exactly once. A contribution outside the bounds is refused with
`launch_input_invalid`, one that cannot be written with
`launch_input_unavailable`; either refuses the launch before a worker starts.

Plugins live outside core. A new runtime package or workflow plugin is never
added to this repository: an official plugin lives in `pohunekd/plugin-<name>`,
a third-party plugin in its author's account, and a plugin becomes installable
by id through a reviewed pull request to `pohunekd/pohunek-plugin-index`, which
needs no core release. The plugin kit, the runtime features, the reusable plugin
workflows, the index and its verifier, and the official plugin snapshot are
available from a later release; the rule applies from now on. Core owns what
plugins build on: the descriptor and manifest schemas, `runtime_api` and the
runtime features, the integration handlers, the plugin kit, the reusable plugin
workflows, the index format and verifier, and the release catalog. A plugin
that needs a new core capability gets it in a core pull request first, behind a
runtime feature. The four
official runtimes (Pi, Codex, Claude Code and Hermes) are the only directories
under `runtime-packages/` and `compat/`, and `cargo xtask docs check` refuses
any other (`plugin-directories`); they are changed in place until they move to
their own repositories ([#1609](https://github.com/zajca/pohunek/issues/1609)).
Naming, required CI, compatibility and admission are in the
[plugin repositories guide](knowledge/guides/plugin-repositories.md).

A runtime feature is a named core capability within one `runtime_api` major
number, with an id such as `input-readiness-v2`. The core keeps an append-only
table of them in `crates/daemon/src/agent/host/features.rs`; a row is never
removed or renamed, and a test compares the table with a snapshot. A descriptor
lists the features it uses in `requires`. An unknown feature, or a gated
descriptor key or compiled parser or handler id whose feature is not listed, is
refused with `runtime_feature_unsupported` before anything is recorded, and an
installed package that fails the check is left out of the load while the other
packages and the daemon start. A feature adds no protocol shape and no
persisted field.

Trust boundary:

- **Same-UID owner trust, not a sandbox.** A descriptor may name any program,
  and that program runs as the owner. The digest or the catalog signature
  proves which bytes were installed and who published them; it does not
  restrict what they launch. A package gains nothing a same-user process does
  not already have.
- **The agent's process tree is not an identity boundary.** Every process in
  an agent's PTY subtree runs as the owner, so a process the agent or one of
  its tools starts can already do anything the agent can. Under
  `NamedForProvider` the worker accepts a descendant's report naming the launch
  process as that process's report, because the Codex and Claude Code hooks are
  such descendants and cannot be told apart from another one; the rule orders
  and attributes reports, it does not authenticate them. On the worker path
  only `FrozenRootEntrypoint` (`identity-v1`) binds the root's report to the
  root's own kernel peer; the public `session.report_native_id` fallback admits
  only the PTY root's own process and refuses `identity-v1` sessions. Under every schema the worker refuses a report from a
  process that is neither the process it names nor a descendant of it
  (`peer_outside_subject`). Authenticated agent
  identity is tracked in [#52](https://github.com/zajca/pohunek/issues/52) and
  isolated runtime profiles in [#88](https://github.com/zajca/pohunek/issues/88).
- **No package code inside the daemon or the session worker.** The daemon
  reads package data into a definition. Integration handlers, hook schemas and
  the reporter scripts the handlers install are compiled core code selected by
  id (`crates/daemon/src/integration/`). The declared program runs only as a
  separate owner process: the agent the worker execs in its PTY from the launch
  plan (`crates/daemon/src/agent/host/launch.rs`), and the bounded version
  probe the daemon spawns (`crates/daemon/src/capabilities.rs`).
- **Local lifecycle only.** `package.*` methods are served on the local control
  socket only (`crates/daemon/src/api/handler/package.rs`, error
  `local_only_method`), and the CLI refuses a remote `--host`
  (`plugin_local_only`). A remote owner connection over the overlay can launch
  only what is already installed and enabled on that host. The `pohunek workflow`
  commands never contact a daemon and refuse a remote `--host` before any I/O
  (`workflow_local_only`); only the reaction dispatcher unit reads the local
  daemon's `subscribe` stream, as a client that never starts it.
- **Workflow plugins are owner code outside core.** The CLI keeps them in its
  own registry root and per-plugin state and config directories, all `0700`,
  and serializes every change under one store lock.
  The digest proves which bytes the owner approved; plugin code runs with the
  owner's full authority and may use every public owner API, as any same-UID
  process can. No daemon, worker, protocol or event surface knows about them.
  `pohunek workflow run` gives an action a minimal environment that keeps the
  complete session origin pair, so `plugin_self_target_denied` still covers an
  action run inside a managed session; a single marker refuses the run. A
  job or service started by the service manager never carries an origin pair
  or the manager's own environment.
- **Remote and relay launches select approved profiles.** A relay-origin launch
  names only a locally approved profile at its approved revision
  (`LaunchSource::RelayProfile` in `crates/daemon/src/agent/host/source.rs`),
  never a runtime id, package, program or argv. A profile pins its package by
  digest, so `plugin update` or `plugin select` does not change what an
  approved profile launches. The relay connection is covered at the resolution
  seam only until [#82](https://github.com/zajca/pohunek/issues/82).
- **Fail closed.** A fresh launch rechecks that the package is enabled and its
  root verifies (`verify_launchable` in
  `crates/daemon/src/agent/host/handle.rs`). A pinned session or profile
  resolves exactly its digest and never falls back to another package or to
  `shell`. There is one exception, for a relaunch only:
  - the resume or fork of a stopped session pinned to an official package may be
    adopted by a strictly newer, selected official version of the same package
    id and runtime id (reserved or not, so Pi too) (`RuntimeHost::newer_adopter`, `recovery_definition` in
    `crates/daemon/src/session/resume.rs`);
  - the newer version must keep the integration, the native reference check and
    the frozen launch contract;
  - a session launched from a profile is adopted only when that profile pins the
    newer version;
  - anything else resolves the exact digest, and a live session is always
    observed through its own.
  "Official" is a registry record from a catalog the host's single anchor
  verified; which root or key authorized it is not recorded
  ([#1200](https://github.com/zajca/pohunek/issues/1200)). `service upgrade`
  repins the profiles pinned to an older version of the release's official
  package. That changes those profiles' revisions, so a relay approval of the
  previous revision no longer matches. Recovery steps are in
  `docs/knowledge/runbooks/runtime-package-recovery.md`.

## Overlay Registry and Discovery

The daemon, SDK, CLI, and external browser gateways consume one configured overlay
registry. Each entry has a stable overlay ID, a transport implementation, and
its own non-zero daemon port. The registry is built by one loader in
`pohunek-client` from the owner-private `<config>/pohunek/overlays.toml`
(`providers`, `port`; absent means NetBird on port 18722) with the
`POHUNEK_REMOTE_PORT` development override. The file and its directory are read
through the trusted-directory checks (owner, no foreign write access, mode
`0600`, at most 64 KiB), and any problem is `overlay_configuration_invalid`
before a provider is consulted. A host runs exactly one provider: the loader
rejects a list of several, and the registry types stay multi-entry for
[#1113](https://github.com/zajca/pohunek/issues/1113). The daemon validates the
file first, before locks, store migration and worker adoption. Daemon listeners run concurrently for every
entry; discovery aggregates providers concurrently while isolating a failed
provider from healthy results. An unqualified selector matching multiple
providers fails closed. A provider-qualified `<overlay>:<selector>` target is
resolved only by that provider and receives its configured daemon port. An
explicit `@<port>` suffix preserves a port learned from discovery while the
selector is still resolved against current provider state. Control and raw
attach reuse the same selected socket route within one SDK client.

Client-generated stable selectors are typed as `peer~<base64url>` or
`fqdn~<base64url>` with no padding. The registry decodes them before provider
resolution and preserves the identity kind, so a stable peer ID is matched only
against the provider's peer-ID field. The provider contract requires a typed
identity resolver and forbids falling back to untyped selector matching, so an
FQDN cannot resolve as another peer's ID or short name. The base64url alphabet
avoids `/`, `+`, `=`, and `@`, keeping selectors unambiguous in session targets,
relay URLs, and the exact-port grammar.

Daemon construction requires a validated registry up front. The shared
discovery cache has no registry-less state, including when the Unix control
server is created through its public constructor.

Discovery preserves provider peer identity separately from display names and
addresses. Stable client identity is overlay-qualified, so equal names,
addresses, or provider IDs cannot collide across overlays. Address-less peers
remain candidates. The public `HostRecord` carries `overlay`, optional
`peer_id`, optional IP-only `address`, and the effective per-overlay `port`.
Provider discovery returns remote peers only; browser, CLI, and other fan-out
consumers add the explicit local Unix-socket target themselves.

A client retains the overlay-qualified peer identity and discovered port, never
the discovered IP as reconnect state. Every control reconnect resolves that
stable identity through current provider state. External attach receives the
same selector with its explicit discovered port; the resulting SDK client keeps
the exact resolved endpoint only long enough to keep control and raw attach on
one route.

CLI fan-out and dynamic completion retain the same canonical identity plus
discovered port, never the cached probe IP. A browser relay forces a fresh local
daemon discovery before each remote tunnel upgrade and accepts the cached route
only if the requested identity still owns it. Active daemon overlay listeners
revalidate their provider-owned local address periodically; an address change
binds the replacement before the stale listener is dropped. The bind itself is
checked in the daemon core: the target must be a unicast address, valid for the
provider's range, and equal to the address the provider reports at that moment,
so a shared address range never decides a bind by itself. A missing provider
CLI disables the listener of the default overlay of an absent
`overlays.toml`; for a provider the owner listed it is logged and retried with
the listener backoff.

The Tailscale adapter (`crates/overlay-tailscale`) is the second compiled
provider. It reads `tailscale status --json` through the same trusted
executable lookup and cancellable subprocess as NetBird, parses only the fields
it needs from the unstable format, and treats the stable node `ID` as the peer
identity. The listener binds the node's IPv4 address and only while Tailscale
runs on a kernel interface; `ShareeNode` peers, expired peers and
peers outside the network map are not selectable, and the ChromeOS and service
addresses of the CGNAT range are never node addresses.

The production NetBird adapter remains tokenless and local. There is no signed
manifest exchange. NetBird `publicKey` or legacy `pubKey` is the stable peer
identity; peers without one preserve `peer_id = null` and clients fall back to
the peer FQDN rather than treating its mutable IP as identity.

1. The daemon/CLI reads local NetBird state via `netbird status --json` (no
   management-API token) to enumerate peers, NetBird addresses, and names.
2. Candidate peers are probed to see which run a reachable `pohunek` daemon.
   A complete discovery, including local status loading, has a bounded deadline;
   each health exchange is separately bounded and the status subprocess is
   cancelled when that deadline expires.
3. Capabilities are obtained by a **live query** to the target daemon over the
   direct NetBird connection (`host inspect`), not from a cached manifest.

NetBird's `status --json` format is treated as an unstable external input: parsed
defensively (optional fields default, unknown fields ignored) and pinned with
recorded fixtures in tests. NetBird/VPN names and addresses are display and
routing hints only. Raw IP selectors must match current peer state, ambiguous
peer names are rejected, and spoofed addresses outside NetBird's CGNAT range
remain non-dialable candidates. Registry-resolving clients never interpret a
socket-address literal as a policy bypass; only routes already validated by
discovery use the explicit trusted-address connection API.

## Projects and Worktree Isolation

The daemon understands *where* a session runs without being told: when a PTY
starts in a git work tree it feels out git and records a lightweight **project**,
keyed by the canonical `git_common_dir` so a repository's main checkout and all
its linked worktrees collapse to one logical project. Projects accrue as a side
effect of working (or via `pohunek project add`); there is no filesystem scan. A
session references a project by `<id|label>` — resolved by the daemon against
**its own** per-host store — so no filesystem path is needed to name a project
on a remote host. The one path a remote start may carry is an explicit
`session new --cwd`, an absolute directory read on that host. See [`docs/design/projects.md`](design/projects.md) for the full
design and the three resolved decisions.

A session binds:

- host;
- project (when started in / pointed at a git repository);
- repository;
- base branch;
- working branch;
- worktree path (worktree sessions only);
- assigned agent;
- logs and events;
- resume binding.

**Isolation is intent-driven.** Without `--branch`, the agent runs **in place**
in the project's checkout as-is — "open a terminal here, work here" — or in the
directory `--cwd` names. With
`--branch`, the daemon creates a **worktree-per-session** off the project's base
branch, so two agents never share one working tree by accident. Worktree
ownership is explicit and recorded in metadata (the binding carries the owning
session and its project); the daemon checks ownership before reusing or cleaning
up a worktree, and `project rm --prune-worktrees` removes only the worktrees
pohunek itself created — never the main checkout or worktrees it did not create.

## State and Recovery Model

Durability tiers (honest about limits):

1. **Client detach:** PTY and process continue because the worker owns them.
2. **Client restart:** session list, metadata, and layout restore; reattach to
   live PTYs.
3. **Daemon restart or crash:** the same worker, PTY, process group, child PID,
   and runtime ID continue. Existing public sockets close; clients reconnect
   after the replacement daemon completes reconciliation.
4. **Worker loss or host restart:** the PTY generation is gone. The logical
   record remains visible as a `failed` session with `runtime.state=lost`
   and no activity; recovery is explicit and creates a new worker, runtime ID,
   PTY, and child PID. A loss counts as ended only when the daemon observed it:
   the service manager's job for the recorded generation ended and was retired
   (including a host restart), or a worker that still answers reports that its
   child process is gone after it started. Every commit of a lost runtime names that proof explicitly;
   it is never derived from the loss reason. A loss without such proof (the
   record names no generation, a recovery whose previous worker still answers,
   an unreachable socket, a sweep that could not confirm the cleanup of marked
   processes: `runtime_lost_cleanup_unconfirmed`) keeps the session state and clears only the activity,
   because no detector is attached to a lost runtime. Startup reconciliation
   re-examines every stored record, so a record committed by an earlier release
   as `running` with a lost runtime is re-classified by its call site's proof;
   a worker that answers again re-adopts the session as `running`. A `failed`
   session with a lost runtime stays resumable (resume keys on the runtime) and
   ages under the lost retention window, aged from the last connection.
   Operations that need a running session still refuse it with
   `session_runtime_lost` and its recovery hint. When the service manager still
   holds the failed worker job, the daemon reads its verdict before it retires
   the job (retirement resets the failed unit and the verdict with it) and
   records it as `runtime.exit`: an open cause string mapped from the manager's
   result (`oom_kill`, `signal`, `exit_code`, `core_dump`, `timeout`,
   `watchdog`, `resources`, `other`) with the signal or exit code of the worker
   process. Only the Linux systemd backend implements the read
   (`Supervisor::exit_evidence`); the launchd backend returns none (#1343), and a
   failed read never blocks the classification. A lost runtime that is
   classified again without a verdict keeps the one it has; any other
   classification of the runtime drops it. A user-requested stop ends the
   runtime `terminal` and carries no exit. The field is additive on the wire
   (no protocol bump; protocol 3 peers do not receive it) and a persisted-shape
   change in the store (store schema 11, a no-op migration from 10). N-1
   readers know both values:
   they treat the session as ended and not stoppable. Rolled back to a release
   that predates this presentation, the stored `failed` record is aged by the
   shorter terminal window when automatic retention is enabled.

Recovery safety rules:

- Never snapshot environment secrets.
- Never run native recovery solely because the daemon disconnected or could not
  negotiate with a worker.
- Reject recovery for a live, reconnecting, conflicting, or incompatible
  runtime.
- Preserve the logical session id and metadata while visibly changing the
  runtime generation.

## Configuration, State, and Log Storage

Configuration under the user config directory:

```text
~/.config/pohunek/
  config.toml
  overlays.toml            # optional overlay provider and remote port (0600)
```

Runtime state under the user data directory:

```text
~/.local/share/pohunek/
  metadata.jsonl           # logical sessions, worktrees, and projects (0600)
  notifications/
    notifications.jsonl    # durable notification action log (0600)
    policy.json             # notification, retention, and compaction policy (0600)
  events/                  # local bounded event journal (audit/debug, not replicated)
  worktrees/               # managed git worktrees
```

Ephemeral owner-private sockets live under one resolved application runtime
root. A valid explicit `XDG_RUNTIME_DIR` resolves to
`$XDG_RUNTIME_DIR/pohunek`; Linux requires it, while macOS without it uses
`/private/tmp/pohunek-<effective-uid>`. An empty, relative, or otherwise invalid
explicit value is an error, and ambient `TMPDIR` never selects the macOS
default.

```text
<runtime-root>/
  daemon.sock
  daemon.lock
  workers/<session-id>/control.sock
```

The resolver validates the complete encoded daemon and worker socket paths,
including the native terminator, before mutating the filesystem. It rejects an
overlong path rather than truncating it. Runtime cleanup never removes durable
session, host identity, approval-key, or governance state.

Structured logs under the user state directory:

```text
~/.local/state/pohunek/logs/
  pohunekd.jsonl[.1-.7]                 # 32 MiB each, 256 MiB daemon cap
  pohunek-session-<session-id>.jsonl    # active worker log
  pohunek-session-<session-id>.jsonl[.1-.3]
                                         # 4 MiB each, 16 MiB per-session cap
~/.local/state/pohunek/workers/<session-id>/<worker-id>.json
```

Durable host identity and governance state is a separate owner-private record
set under the same state root:

```text
~/.local/state/pohunek/host/
  identity.json      # stable opaque HostId (0600)
  approval.key       # private approval signing material; never public API data (0600)
  governance.json    # current enrollment/owner state and bounded retired data (0600)
  state.lock         # cross-process host-state lock (0600)
```

The `host/` directory is mode `0700`. The daemon requires those exact
owner-private directory/file modes, refuses unsafe path
bindings, serializes host-state writers through `state.lock`, and uses atomic
replacement plus synchronization. A post-rename synchronization failure is a
durability-uncertain result, not a successful update: in-memory governance is
discarded and can be used only after an explicit serialized reload.

Daemon and session-worker logs rotate before a complete JSON event would exceed
the per-file limit. All worker generations for one logical session serialize
through one owner-private file lock and share the 16 MiB family cap. Removing a
session first stops its worker and then removes that session's regular log
files. Startup pruning removes owned oversized rotations and the daemon's
legacy `pohunekd.log.YYYY-MM-DD` files, but never follows or deletes symlinks.
An individual event larger than one file is replaced by a small valid JSON
warning (or dropped when even that warning cannot fit), never partially
written. There is intentionally no machine-wide cap across simultaneously live
sessions; each live session has an independent bounded diagnostic history.

The metadata store is a **single** owner-private JSON-lines file whose lines are
internally tagged by `kind` — `session` (logical intent, launch and runtime
binding), `worktree`, and `project` — sharing one
serialization lock and one atomic temp+rename write path, so a write of one
record kind can never corrupt or drop another and any single update is
crash-atomic. The event log is the local audit/debug trail: `events/events.jsonl`
rotates by size and age under the `[events]` retention of `service.toml`
(`max_file_bytes`, `max_files`, `max_age_days`), omits `agent_state` events that
restate a session's last journaled state, and is written by one dedicated
thread so no tokio worker waits on the filesystem. None of these is
replicated across hosts by the shipped owner paths — each host's daemon is
authoritative and is answered live (see "High-Level Architecture"). The
planned relay may persist a reconstructible, explicitly stale session-catalog
projection in PostgreSQL, but never terminal bytes or authoritative host state.
An embedded host-local SQLite `state.db`
(schema-versioned, with forward migrations) is a **deferred** option, to be
adopted only if scale or query needs justify it; the schema is sketched in
`docs/plan-phase-1.md` ("Deferred: SQLite Schema").

The notification store is a separate owner-private append-only JSONL action
log. A daemon-owned maintenance task applies policy TTLs to quiet, resolved, and
archived records; unread/read action-required and error records are never
age-pruned. Once the configured action threshold is reached, maintenance writes
one current action per non-deleted record to a fully flushed temporary file,
opens it for future appends, atomically renames it over the old log, and syncs
the parent directory. A crash therefore leaves either the old replayable log or
the complete compacted log, never a partially replaced store.

Secrets are never written to the metadata stores, the event log, or session
metadata. They
stay in the OS keychain, provider CLIs (`gh`, etc.), the SSH/agent environment,
or explicit local environment files that are not committed.

Background-task labels and shell program names are not secrets, and they are
kept on purpose (operator decision of 2026-10-12 in
[#890](https://github.com/zajca/pohunek/issues/890)). They are written to the
worker journal, to the session records of `metadata.jsonl`, and to the event log
(`events/*.jsonl`, in the `session_updated` and `background_state` payloads),
and clients and the relay receive them. A shell command and its arguments are in
none of these sinks: the worker keeps only the basename of the command word, and
the worker and daemon logs record counts only.

The worker journal contains runtime identity, process identity, dimensions,
phase, terminal outcome, sanitized identity claims, and output offsets. It does
not contain environment values, prompts, input bytes, terminal bytes, rendered
screens, tokens, or notification bodies. The accepted task layer adds a
separate owner-only **evidence journal** beside it, trimmed on daemon
acknowledgement, that may briefly hold provider-reported text such as a final
assistant message under the same posture as scrollback (accepted, not
implemented; [#182](https://github.com/zajca/pohunek/issues/182), worker part
[#220](https://github.com/zajca/pohunek/issues/220)). Live output history and
terminal screens remain bounded in worker memory.

## Security Model

### Shipped owner-path trust domain

The shipped v4 threat model is one operator on their own machines and configured
private overlays. It has no application-level user authorization. The controls
that remain are cheap, free-by-default, or inherited:

- **Local socket** in a `0700` directory with mode `0600`: owner-private, no
  authorization system needed.
- **Remote listener bound only to the configured overlay's address**, never
  `0.0.0.0`. The overlay (WireGuard in both NetBird and Tailscale) provides
  encryption and network authentication; the overlay's policy (NetBird policies,
  tailnet ACLs) decides which peers can reach the daemon port.
- **Origin-session guard.** Requests from managed children carry paired
  `origin_session_id` and `origin_daemon_id` envelope markers. When both identify
  the target as the caller's origin, the daemon rejects exactly `session.stop`,
  `session.resume`, `session.remove`, `session.remove_accepting_unconfirmed`,
  `session.fork`, `session.resize`, `session.set_metadata`, `session.rename`,
  and `session.input` with `plugin_self_target_denied`. Read-only observation is allowed. The lifecycle
  reports `session.report_agent`, `session.release_agent`, and
  `session.report_native_id` are also deliberately allowed: managed hooks must
  report their own session. The public `session.report_native_id` method
  attests its caller with the kernel peer of the local connection: it admits
  only the session's PTY root itself, and refuses every report of an
  `identity-v1` session, so a hook's fallback to it is refused and the
  owner-private worker claim is the route for a native reference. The
  pair is atomic and is copied to dedicated wait connections. This narrowly
  prevents an in-session automation client from invoking those nine mutations
  against the PTY that hosts it, but it is not authentication: any same-user
  process able to reach the owner socket remains inside the trusted owner-path
  boundary.
- **Prompt-injection / confused-deputy risk is inherited from the agents.** An
  operator agent that reads attacker-influenced text (a malicious issue, PR, or
  repository content) and then acts is a risk you already accept by running Codex
  / Claude Code. It is mitigated by those agents' own approval gates, not by a
  new authorization layer in `pohunek`. Dangerous operations (e.g. starting
  sessions on other hosts) stay behind explicit, per-action confirmation.
- **Secrets in terminal output are accepted, not guaranteed-redacted.** Agents
  may print tokens to the PTY; scrollback on your own disk may therefore contain
  secrets. The "no secrets" guarantee applies to *structured* metadata
  (the metadata stores, events, config), not to raw terminal streams. Scrollback is stored
  owner-private; sessions may opt out of scrollback persistence.

External text (terminal output, provider responses) is data, never instructions
to the control plane.

Direct-host profiles execute under the daemon owner's Unix account. A malicious
repository, command, agent, or collaborator inside such a session is not an
isolated workload; same-UID access remains within the owner trust boundary.
Profile-backed container and VM isolation is later work in
[#88](https://github.com/zajca/pohunek/issues/88).

### Accepted team-relay trust domain

The relay process and operator are trusted with transient plaintext and the
union of every active `HostShare`. Relay application RBAC can deny an
infrastructure administrator ordinary API access to session content, but it
does not protect against a compromised relay process or its operator.

The relay authenticates human principals and service accounts and authorizes
teams, groups, roles, and session ACLs. The daemon deliberately does not repeat
those decisions. It authenticates only its enrolled relay and fails closed on
the locally authoritative share, allowed operation, project/worktree root,
profile, resource limits, and immutable session origin. Thus a compromised
relay can exercise everything allowed by active shares, but cannot use the
relay path to enumerate or control owner-origin sessions or exceed local share
ceilings.

Only relay-created session traffic may pass through the relay. The relay never
persists PTY output, input, prompts, terminal snapshots, file contents, or raw
secrets; audit and PostgreSQL contain bounded structured metadata only. The full
claims, non-claims, data classification, and revocation model are specified in
the [team-relay RFC](design/team-relay-control-plane-rfc.md).

## Observability

Structured logs under `~/.local/state/pohunek/logs/` use the bounded retention
described in "Persistence and Local Data" and redact secrets and sensitive
terminal content. Useful signals:

- Daemon startup/shutdown, reconciliation, and single-instance/socket recovery.
- Worker bootstrap, controller lease, runtime identity, output-gap, child-exit,
  terminal acknowledgement, and shutdown outcomes.
- Control request summaries (with correlation `id`) and response status.
- Session start, attach, detach, stop, process exit, daemon reconnection, worker
  loss/conflict, and explicit native recovery.
- PTY allocation, resize, stream errors, worker protocol versions, and
  controller reconnect latency.
- Overlay discovery runs and candidate/capability results. The CLI can discover
  locally without `pohunekd`; daemon discovery remains available for browser and other RPC consumers.
- Agent state transitions with their `source`.
- Latency for CLI commands, attach, discovery, and remote connections.

`pohunek doctor` reports environment, daemon, each configured overlay, agent CLI,
and storage health for humans and operator agents.

## Error Handling

- Fail closed for resume decisions that would auto-run custom commands.
- Keep unrelated sessions running when one session fails.
- Preserve which host failed in remote error messages.
- Provide suggested recovery commands when known.
- Return structured error data for `--json` commands.
- Distinguish error classes: configuration, daemon (unavailable, version
  mismatch, framing), transport (overlay unreachable, connection lost), runtime
  (agent binary missing, PTY allocation, process exit, worktree conflict), and
  discovery (overlay CLI missing, local state unavailable).

### Unknown enum values from a newer daemon

A daemon of a newer build can answer on the same protocol version with an enum
value an older Rust client does not know. The mechanism is strict by default and
tolerant only where the client opts in:

- Plain `serde_json::from_*` rejects an unknown value everywhere: daemon
  params, the session store, the notification store, governance state and
  config files.
- A tolerant enum has a hidden `Unknown` variant (`ts(skip)`, so the generated
  TypeScript types do not change). Only the entry points `protocol::tolerant::
  from_value`, `from_str` and `from_slice` map an unknown string to it. They set
  a thread-local flag for one synchronous decode call and restore it on exit,
  so the mode can never be held across an `.await`.
- `Unknown` serializes as the fixed token `"unknown"`, so `--json` output and
  the `Client::call` round trip keep working. Plain decoding rejects that token
  too. The daemon decodes its params, stores and config strictly and never
  holds an `Unknown` in its own state. Its host discovery probes peer daemons
  through the client code, so a peer reply is decoded tolerantly there; the
  verdict reads only the version and whether the reply is a success, never an
  enum value, and no `Unknown` is stored or emitted.
- The enum's serde derive is the only definition of its wire names; the
  `tolerant_enum!` macro adds the strict-or-tolerant `Deserialize` around it.
- The Rust client, the CLI and the assistant decode every daemon response and
  event through the tolerant entry points.
- `SessionWarningKind` keeps its own mechanism: an unknown warning is dropped on
  decode.
- The TypeScript SDK follows the same rule per guard: a daemon error response
  whose `class` is a string this build does not define is decoded with the class
  `"unknown"` (`ObservedErrorClass`), keeping `code`, `msg` and `recover`. The
  generated `ErrorClass` type is unchanged. A later slice that makes a Rust
  enum tolerant decides its TypeScript guard by this rule; a bump-only enum
  stays strict in TypeScript too.

Rejected alternatives: a derive-level `#[serde(other)]` makes persisted and
param decoding fail open; a Cargo feature is switched on in the daemon too by
workspace feature unification; opt-in strictness at daemon sites fails open when
a site is missed; a raw-string-preserving `Other(String)` costs `Copy` on about
45 enums.

Each enum has one policy category:

- **display**: shown or emitted as `unknown`.
- **drop**: the element is removed from its collection on decode.
- **safe**: `unknown` plus a defined conservative branch.
- **bump-only**: stays strict. A new value needs a protocol bump: the N-1
  adapter maps or withholds it, or `MIN_PROTOCOL_VERSION` rises.
- **n/a**: not decoded by the Rust client from the daemon wire.

An `Unknown` never triggers an effect: no reinstall, no downgrade, no
auto-select, no consent skip, and no "free to remove" or "not running" verdict.
It never skips a guard, consent or check, and never asserts a safe or finished
state. A daemon-side `match` arm for `Unknown` takes the conservative arm and
never `unreachable!()`.

| Enum | Policy | Behaviour of `Unknown` | Slice |
|---|---|---|---|
| `ErrorClass` | display | `code`, `msg` and `recover` survive; `--json` keeps the envelope | done |
| `DoctorStatus` | safe | ranks as warn in `worse()`, never `ok` | done |
| `IntegrationFindingCode`, `IntegrationFindingSeverity`, `IntegrationUninstallState` | display | | done |
| `IntegrationInstallState` | safe | never `Current` | done |
| `IntegrationRecovery` | safe | never `None`; generic hint | done |
| `AgentActivity` | display, safe | an input-wait miss is the existing typed error | done |
| `StateSource`, `CwdSource`, `SubagentLifecycle` | display | a lifecycle `unknown` is not counted as running | done |
| `BackgroundKind`, `BackgroundStatus` | display | maps to the existing `Other` | done |
| `RuntimeState` | safe | may own a runtime, cannot be stopped or removed, blocks upgrade | [#1336](https://github.com/zajca/pohunek/issues/1336) |
| `RuntimeInventoryStatus`, `SessionRetentionReason`, `SessionWaitTurnReason`, `SessionReadSource` | display | | [#1337](https://github.com/zajca/pohunek/issues/1337) |
| `SessionRetentionHold` | safe | never `None` (free to remove) | [#1337](https://github.com/zajca/pohunek/issues/1337) |
| `SessionWaitReason` | safe | reported like a timeout, non-zero exit | [#1337](https://github.com/zajca/pohunek/issues/1337) |
| `DetectionRegionKind` | drop, display | dropped from `supported_regions` | [#1337](https://github.com/zajca/pohunek/issues/1337) |
| `PackageOrigin` | safe | the downgrade guard stays engaged | [#1338](https://github.com/zajca/pohunek/issues/1338) |
| `PackageBindStatus` | safe | never `Unchanged`; consent is still asked | [#1338](https://github.com/zajca/pohunek/issues/1338) |
| `PackageFault`, `PackageSelectionBlockReason`, `PackageInstallStatus` | display | | [#1338](https://github.com/zajca/pohunek/issues/1338) |
| `PackageFindingKind` | display | the finding is kept, so the doctor exit stays non-zero | [#1338](https://github.com/zajca/pohunek/issues/1338) |
| `NotificationSeverity`, `ProjectSource`, `ProviderKind`, `PromptLayer`, `ConceptType`, `QuarantineReason` | display | | [#1339](https://github.com/zajca/pohunek/issues/1339) |
| `NotificationStatus` | safe | never matches `--unread` or `--status` filters | [#1339](https://github.com/zajca/pohunek/issues/1339) |
| `ConceptIntent` | drop | | [#1339](https://github.com/zajca/pohunek/issues/1339) |
| `SessionWarningKind` | drop | own mechanism | done |
| `SessionState` | bump-only | drives live and terminal decisions; no safe default | |
| `SessionTurnEventKind` | bump-only | the daemon emits only the kinds the client asked for | |
| `NotificationKind` | bump-only | a field of the `deny_unknown_fields` struct `NotificationKindPolicy` ([#1332](https://github.com/zajca/pohunek/issues/1332)) | |
| `EnrollmentStatus`, `HostOwner` | bump-only | cross-validated with quarantine; an identity coordinate covered by a signed outcome | |
| `ConceptDeprecation` | bump-only | untagged; any new version string already decodes | |
| `HostOwnerKind`, `ShareSuspensionIntent`, `TransferOutcomeError`, `PackageTrust`, `BindingProvenance`, `HostClass` | n/a | not decoded from the daemon wire by the Rust client; must stay strict | |

0.38 rule: a 0.38 change adds a value only to an enum that 0.37 already
decodes tolerantly in both the Rust client and the TypeScript SDK
(`AgentActivity`, `CwdSource`, `SubagentLifecycle`, `BackgroundKind` and
`BackgroundStatus`), and optional fields only to non-strict types. A 0.37 client
is still strict for an enum whose tolerance ships in 0.38, and the 0.37
TypeScript SDK checks `StateSource` against a closed set, so a new value of
either needs a protocol bump and an N-1 adapter. A new value of a bump-only
enum (`SessionState`, `SessionTurnEventKind`, `NotificationKind`) needs a
protocol bump too, which 0.38 does not make, so those get no new value in 0.38.

## Testing Strategy

Tests protect externally meaningful behavior by driving cooperating
production components through supported boundaries (integration) or the real
product processes (E2E); there is no unit-test tier, whatever file a scenario
lives in. The full boundary, the migration state of the unit-tier tests still
in the suite ([#671](https://github.com/zajca/pohunek/issues/671)), and the
expected-result rules are in the
[verification guide](../.agents/instructions/verification.md#testing-policy).
The obligations below must each stay represented by integration or E2E
scenarios.

Integration and E2E obligations:

- Control protocol serialization for request/response/error/event envelopes and
  version negotiation, exercised against the daemon and worker ends.
- Session lifecycle with controlled PTY programs: start, attach, detach, resize,
  reattach, process exit, stop.
- Graceful daemon restart and `SIGKILL` preserve worker PID, child PID, PTY,
  runtime ID, output continuity, input, resize, detection, and stop behavior.
- Worker loss affects only one session and becomes an explicit lost runtime;
  duplicate or mismatched workers become conflicts without automatic killing.
- Separate attach-stream connection: raw bytes survive arbitrary content; control
  actions work while attached.
- Agent state mapping from hook signals (with deterministic fixtures), including
  the `blocked` case; heuristic fallback.
- Explicit recovery via native session IDs creates a new runtime generation.
- Worktree binding, ownership checks, and conflict handling.
- Session transaction, worker-journal, reconciliation, and metadata-store
  durability across create, stop, remove, recovery, and restart, including
  persisted-schema guards and migration of stored records.
- CLI command behavior: arguments to request, table and `--json` output, error
  output, and exit status.
- Local daemon + CLI full session lifecycle driving the real `pohunekd`,
  `pohunek-sessiond`, and `pohunek` binaries.
- Remote daemon over an overlay (or a loopback TCP stand-in) reusing the same
  protocol; attach/detach without killing the remote process.
- Tokenless NetBird and Tailscale discovery from `netbird status --json` and
  `tailscale status --json` fixtures, with
  graceful degradation when the CLI/state is absent.
- Structured logs and event-log records for important transitions.

## Architecture Risks

- **PTY/TUI state detection.** Mitigation: prefer agent hooks over scraping; mark
  state `source`; validate the `blocked` signal empirically.
- **Worker loss destroys one PTY generation.** Mitigation: one isolated
  non-restarting worker per session, durable logical records, explicit lost
  state, and optional operator-triggered native recovery.
- **Ambiguous runtime identity.** Mitigation: validate the supervisor job's
  process, its executable and arguments, the process start identity, and
  journal fields; quarantine conflicts and never
  automatically kill an ambiguous live worker.
- **NetBird local state format drift.** Mitigation: defensive parsing + recorded
  fixtures; shell out to the documented `status --json` rather than internals.
- **Tailscale status format drift.** Mitigation: the `tailscale status --json`
  format is documented upstream as subject to change; the parser reads only the
  fields it needs, tolerates unknown fields, and is pinned with spec-derived
  fixtures.
- **Daemon as a network server on an overlay.** Mitigation: bind only to the
  overlay's address; rely on the overlay's policy (NetBird policies, tailnet
  ACLs); no `0.0.0.0`.
- **Worktree cleanup conflicts.** Mitigation: explicit session ownership and
  recorded bindings; ownership checks before reuse/cleanup.
- **Embedded terminal maturity.** Mitigation: the CLI attach path needs no embedded terminal; the native GUI
  lives in `zajca/pohunek-work`.
- **Agent CLIs change under us.** Mitigation: keep the agent boundary thin; rely
  on documented modes (hooks, native resume) and pin behavior with fixtures.

## What Changed vs. the Original Plan (`idea.md`)

| Area | Original plan | This revision |
|------|---------------|---------------|
| Scope | Multi-host, team-capable | Owner-first direct operation plus an accepted optional multi-team relay |
| Remote transport | SSH bridge | Direct over the configured overlay (NetBird default, Tailscale opt-in) |
| Discovery | Tailscale + NetBird + signed manifests | Overlay-local status + live capability query |
| Mesh trust | Signed manifests, key rotation, snapshot sync | Owner paths use overlay + filesystem permissions; relay uses explicit enrollment and local `HostShare` ceilings |
| Audit | Tamper-evident considered | Plain local event log today; durable relay audit and admission foundation implemented in [#85](https://github.com/zajca/pohunek/issues/85), with operational retention and load evidence in [#87](https://github.com/zajca/pohunek/issues/87) |
| Agent state | Terminal heuristics | OSC title + screen-manifest + PTY activity for parent state; hooks capture recovery identity and durable Claude/Codex subagent lifecycle |
| Providers | In-tree Linear/GitHub adapters | Deferred, shell-out (`gh`, Linear GraphQL/MCP) in the client surfaces, not the chassis |
| GUI | libghostty client (MVP5) + spike (MVP0) | Mesh-local browser client shipped (the browser and native desktop clients live in `zajca/pohunek-work`); the HTTPS native relay credential CLI is implemented, while the full team client and UI remain [#86](https://github.com/zajca/pohunek/issues/86) |
| Attach framing | "separate stream mode" (unspecified) | Separate connection per PTY (specified) |
| Agents | Codex + Claude Code | Codex + Claude Code + local-terminal Hermes Agent 0.20.0 and later 0.x |
