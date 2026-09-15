# Complete macOS Host and Client Support RFC

Status: accepted product contract; platform foundation implemented by #95;
complete macOS support remains deferred through #96-#105.

## Outcome and support contract

Pohunek will support a Mac as a first-class owner host and client without
weakening its owner-first, direct-overlay, or PTY/TUI-first architecture. The
complete release includes the CLI, daemon, independent session workers, native
Iced GUI, retained owner WebUI, Codex, Claude Code, the pinned Hermes runtime,
and direct communication with Linux hosts over a configured overlay.

The release targets native `aarch64-apple-darwin` and
`x86_64-apple-darwin` artifacts without a Rosetta dependency. The proposed
minimum is macOS 14.0. CI pins `MACOSX_DEPLOYMENT_TARGET=14.0`; this compile-time
floor is not a substitute for the native minimum-OS acceptance gate in #105.
Supported current versions must also be named and tested at release time.

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

Linux keeps its required runtime-directory contract. On macOS, when no valid
explicit `XDG_RUNTIME_DIR` exists, the shared default is the short owner path
`/private/tmp/pohunek-<effective-uid>`; ambient `TMPDIR` does not select it. The
implementation must validate the trusted parent and descriptor-relative owner,
type, and mode of every pre-existing entry. It must reject unsafe symlinks or
foreign entries rather than repairing or deleting them. Private directories use
mode `0700`, and private sockets/files use `0600` unless an established
executable contract requires otherwise.

All clients, workers, hooks, launchd jobs, and the Bun backend resolve the same
runtime path. The complete encoded socket path must fit Darwin's Unix-socket
limit before any mutation. Cross-process locks, descriptor-relative checks,
atomic replacement, atomic no-replace installation, and file/directory sync
retain their current security and durability meaning. An uncertain durability
result remains an error.

## Native identity, PTY, and supervision

Darwin process inspection must provide PID, parent PID, UID, opaque start
identity, executable, argv, cwd, foreground process group, required ownership
markers, and bounded inventory. Exit observation uses kqueue process events with
registration/recheck logic that rejects exit and PID-reuse races.

Unix peer credentials come from the accepted socket before request dispatch.
Darwin uses `LOCAL_PEERCRED` and `LOCAL_PEERPID`, or an equally strong supported
kernel API. Same-owner checks, process-start checks, launch ancestry, frozen
provider binding, lease, sequence, expiry, and immutable identity rules remain
unchanged. Request fields and UID alone cannot establish a PID-bound claim.

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

Native shared-contract CI runs on pinned Apple Silicon and Intel macOS runner
labels, verifies `uname -m`, uses the locked graph, treats warnings as errors,
and compiles/tests `pohunek-platform` with the macOS 14 deployment target. Full
application and release gates are added with their real native backends; shared
contract CI must not masquerade as complete host support.

## Ordered delivery

1. #95 defines shared contracts, migrates Linux behavior, isolates target
   dependencies, and establishes native Darwin library CI.
2. #96 adds secure macOS runtime paths and portable durable filesystem
   operations.
3. #97 implements native process inspection and race-safe exit observation.
4. #98 preserves trusted Unix peer and agent identity.
5. #99 makes worker PTY I/O and attach behavior portable.
6. #100 adds launchd supervision for independent durable workers.
7. #101 ports transcript observation and agent integration lifecycles.
8. #102 adds CLI setup, desktop integrations, and actionable diagnostics.
9. #103 validates owner WebUI and direct-overlay workflows.
10. #104 ships native installation, upgrades, signing, and notarized artifacts.
11. #105 closes the release with native durability, security, minimum/current
    OS, cross-architecture, and cross-stack acceptance.

These are engineering milestones within one complete release scope. No
intermediate merge advertises partial macOS support as a reduced product.

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
