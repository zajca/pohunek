# Complete macOS Host and Client Support RFC

Status: accepted product contract; platform and secure-path foundations
implemented by #95 and #96; complete macOS support remains deferred through
#97-#105.

## Tracking authority

This RFC records accepted product and architecture constraints. It is not the
delivery tracker. The issue hierarchy rooted at
[#94](https://github.com/zajca/pohunek/issues/94), the
[`Complete macOS support` milestone](https://github.com/zajca/pohunek/milestone/2),
and the [macOS project](https://github.com/users/zajca/projects/4) are the source
of truth for implementation scope, dependencies, sequencing, and status. When
tracking data differs from this document, GitHub tracking wins; changes to the
product or architecture contract still require this RFC and canonical docs to
be updated.

## Outcome and support contract

Pohunek will support a Mac as a first-class owner host and client without
weakening its owner-first, direct-overlay, or PTY/TUI-first architecture. The
complete release includes the CLI, daemon, independent session workers, native
Iced GUI, retained owner WebUI, Codex, Claude Code, the pinned Hermes runtime,
and direct communication with Linux hosts over a configured overlay.

The release targets native `aarch64-apple-darwin` artifacts without a Rosetta
dependency. Intel Macs and `x86_64-apple-darwin` artifacts are outside the
current release scope; adding them requires a separate accepted scope and native
acceptance evidence. The proposed minimum is macOS 14.0. CI pins
`MACOSX_DEPLOYMENT_TARGET=14.0`; this compile-time floor is not a substitute for
the native minimum-OS acceptance gate in #105. Supported current versions must
also be named and tested at release time.

Production services run as the logged-in owner through launchd user agents.
Closing a terminal or locking the screen does not stop workers. Logout may end
the launchd login domain, and reboot cannot preserve live processes. Pohunek
must report those boundaries honestly and retain only durable metadata and
explicit recovery paths.

Unattended boot before login, root-owned system services, a privileged helper,
Windows support, and hostile-workload isolation are separate scopes. Linux-only
sway/rofi integration remains an optional platform-specific capability.

## Architectural boundaries

`crates/platform` owns narrow target-neutral contracts for process facts,
stable same-boot process identity, kernel-derived Unix peer identity, and native
service supervision. It contains concrete OS implementations but no session
state, provider policy, public protocol, GUI state, or network routing policy.
Unsupported target operations are absent at compile time or fail with a typed
error; there is no production success stub.

Process start identities are opaque equality tokens, not portable timestamps.
Persisted uses must bind them to the relevant boot identity. Only required
process facts and allowlisted Pohunek ownership markers may leave the platform
layer. Complete process environments never enter logs or public responses.

Daemon runtime policy remains in `crates/daemon/src/runtime`. Its supervisor
interface uses validated logical service IDs, portable states, and optional
stable process identities. systemd unit names, D-Bus paths, launchd labels, and
raw main-PID semantics remain backend details. Starting, bounded discovery,
inspection, explicit replacement, and targeted retirement are required
operations. A supervisor activation result is not worker readiness or identity
authority; the private worker handshake remains authoritative.

Standalone Unix-socket and direct-overlay operation remain first-class. NetBird
is the production overlay. macOS support does not add SSH transport and does
not require the optional team relay.

## Secure paths and durable files

Existing XDG config, data, state, and cache precedence remains consistent. A
port must not relocate durable host identity or create a second `HostId`.

Linux keeps its required runtime-directory contract. On macOS, when an explicit
`XDG_RUNTIME_DIR` is absent, the shared default is the short owner path
`/private/tmp/pohunek-<effective-uid>`; ambient `TMPDIR` does not select it. The
implementation must validate the trusted parent and descriptor-relative owner,
type, and mode of every pre-existing entry. It must reject unsafe symlinks or
foreign entries rather than repairing or deleting them. Private directories use
mode `0700`, and private sockets/files use `0600` unless an established
executable contract requires otherwise.

All clients, workers, hooks, launchd jobs, and the Bun backend must resolve the
same runtime path. #96 provides the shared contract and fixtures; #103 owns the
Bun consumer integration. The complete encoded socket path must fit Darwin's
Unix-socket limit before any mutation.

Durable replacement writes and synchronizes the temporary file before its
atomic rename, then synchronizes the containing directory. Atomic no-replace
installation commits only when the destination was absent at that instant and
reports a collision distinctly. Failure to synchronize the directory after a
rename is an uncertain committed result, not success; callers retain their
existing fail-closed recovery behavior. Cross-process locks and
descriptor-relative ownership, type, mode, no-follow, and inode checks retain
the same meaning on Linux and APFS.

## Native identity, PTY, and supervision

Darwin process inspection must provide PID, parent PID, UID, opaque start
identity, executable, argv, cwd, foreground process group, required ownership
markers, and bounded inventory. Exit observation uses kqueue process events with
registration/recheck logic that rejects exit and PID-reuse races.

Unix peer credentials come from the accepted socket before request dispatch.
Darwin uses `LOCAL_PEERCRED` for the owner and `LOCAL_PEERPID` for the process
id. Same-owner checks, process-start checks, launch ancestry, frozen provider
binding, lease, sequence, expiry, and immutable identity rules remain unchanged.
Request fields and UID alone cannot establish a PID-bound claim, and a peer the
kernel cannot attest is an explicit rejection rather than a downgrade.

Peer identity has two halves with different guarantees, and the contract names
them separately. The owner is **connection-time** identity: both kernels freeze
it when the connection is established, Linux in the socket's `SO_PEERCRED`
record and Darwin by copying the connecting process's credentials into the
accepted socket inside `unp_connect`. The process id is **inspection-time**
identity: Linux freezes it with the rest of `SO_PEERCRED`, but Darwin serves
`LOCAL_PEERPID` from the peer socket's `last_pid`, which the kernel re-stamps
whenever a different process operates on that socket, so a descriptor inherited
across `fork`/`exec` or passed over `SCM_RIGHTS` changes the reported peer.
Services therefore capture the peer before parsing a request and re-read the
kernel answer before every decision that grants authority; a drift is a typed
rejection. Because a connection outlives the moment it was authorized, that
means every request on a leased control connection, every frame of a live data
stream including the deferred page an observation stream releases after its
wait, and a timed re-check while a leased connection is silent — an exclusive
lease held by a descriptor inherited from an exited daemon would otherwise block
recovery. A peer that exited
without being reaped is not live either: a zombie keeps its process id and
start identity, so liveness is asked of the process record rather than inferred
from identity alone. An in-place `exec` stays indistinguishable, because it preserves both
the process id and the kernel start time, and executable identity remains the
launch-claim path's concern rather than the transport's.

`LOCAL_PEERTOKEN` was evaluated as the stronger alternative and rejected. The
kernel resolves it through the same `last_pid`, so it does not detect a
descriptor hand-off either; `audit_token_t` is absent from `libc`, its field
layout is undocumented, and `libbsm` has been deprecated since macOS 11. The
shared process start identity already supplies process-reuse protection on both
targets.

A private identity report is accepted only from the process it names or from a
descendant of it. That is the shape every shipped hook already has — Codex and
Claude run their hook as a child of the agent whose id they report, and the
pinned Hermes plugin reports from the agent process — and it keeps an unrelated
command in the same terminal from rewriting another process's identity, which
matters because the PTY root's environment carries the runtime id and the
private socket path. The stricter `peer == subject` binding belongs to the
explicit in-process identity mode in
[#52](https://github.com/zajca/pohunek/issues/52), which can require it on a
wire shape of its own without breaking child hooks. Note that #98's test list
also asks for "a child claiming a parent PID" to be rejected while its required
behavior asks for existing child hooks to keep working; those cannot both hold
for the shipped integrations, and this RFC records the reading that preserves
them.

The public `session.report_native_id` fallback, which a hook uses when the
private worker socket is unreachable, carries no kernel peer binding. It keeps
its existing runtime, ordering, expiry, and provider rules, and hardening it is
separate work.

The daemon and every worker are separate sibling launchd jobs in the owner's
login domain. Worker definitions are private, explicitly registered, and do not
resurrect stale sessions automatically. Jobs use executable paths and argument
arrays without shell interpolation. Restarting or upgrading the daemon must
preserve the live worker PID, child identity, PTY, and output drain. Failed or
cancelled supervisor operations reconcile observed state before retrying so an
uncertain commit cannot create a duplicate generation.

PTY work retains `portable-pty` and the existing attach protocol while replacing
epoll-only readiness. Output quiescence, bounded backpressure, replay ordering,
read-after-exit drain, input deduplication, and resize ordering are invariants.
The subprocess launcher remains an integration harness, never production
durability.

## Clients, integrations, and distribution

Agent integration install/status/doctor/uninstall behavior must be real on
macOS, including no-clobber transactions and rollback. A missing runtime is not
a passing compatibility test. Client work includes a deliberate shell/PATH
policy, terminal launch, native key bindings, Keychain-backed credentials,
notifications, browser, and clipboard integration. The GUI keeps the existing
headless `gui-core` split.

The owner WebUI remains the Bun gateway with private binding, origin,
WebSocket, and reconnect protections. Release artifacts are native, signed, and
notarized. Gatekeeper verification, upgrades with live sessions, and uninstall
behavior are part of release acceptance, not follow-up polish.

## Dependency and target audit

The #95 lockfile review records these relevant versions: `portable-pty` 0.9.0,
`nix` 0.28.0/0.29.0, `rustix` 0.38.44/1.1.4, Iced 0.14.0, Keyring 3.6.3,
`zbus` 4.4.0/5.16.0, and `reqwest` 0.12.28 with rustls. Keyring's Secret
Service backend and systemd D-Bus dependencies are Linux-only; Keyring's
Apple-native backend selects Security Framework on Darwin. Iced's Wayland and
Linux theme-detection features are Linux-only while the portable renderer stays
available. File locking still uses reviewed OS primitives rather than a new
cross-platform locking dependency.

Native shared-contract CI runs on a pinned Apple Silicon macOS runner label,
verifies `uname -m`, uses the locked graph, treats warnings as errors, and
compiles/tests `pohunek-platform`, `pohunek-paths`, and the portable filesystem
contract with the macOS 14 deployment target. Full application and release
gates are added with their real native backends; shared contract CI must not
masquerade as complete host support.

## Delivery model

The GitHub tracker orders the work from the completed shared platform and secure
path foundations through native process and peer identity, PTY portability,
launchd, integrations, clients, distribution, and final native acceptance.
These are work items within one complete release scope. No intermediate merge
advertises partial macOS support as a reduced product, and #105 remains the
release closure gate.

## Release acceptance

The final gate exercises local create/attach/input/resize/stop, terminal and UI
closure, graceful and forced daemon restart, upgrade, worker failure isolation,
PID reuse, forged identity reports, unsafe paths, concurrent installers, disk
failures, logout/reboot, sleep/wake, NetBird outage and reconnection, native GUI,
owner WebUI, all supported agents, and install/upgrade/uninstall with live
sessions. It records OS, architecture, hardware, deadlines, and resource use,
including at least 20 concurrent sessions and five simultaneous attaches.

Missing native hardware, signing credentials, launchd login context, or pinned
agent runtime is a visible open gate, never a green skip. Public macOS support is
declared only after #105 is complete.
