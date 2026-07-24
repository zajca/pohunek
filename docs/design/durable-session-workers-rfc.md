# RFC: Durable Session Workers

- **Status:** Implemented; merge-ready
- **Date:** 2026-07-24
- **Scope:** Linux session-runtime ownership, daemon recovery, and packaging
- **Audience:** maintainers of `pohunekd`, the Rust clients, release tooling, and
  the systemd user-service installation

## Implementation Status as of 2026-07-24

The implementation exists on the `zajca/durable-session-workers-rfc` branch and
satisfies this RFC's Definition of Done. Every remaining pre-merge item now has
executable evidence (see "Validation evidence" below). This section is the
authoritative record of the completed state. Normative requirements in the rest
of the RFC remain unchanged.

### Implemented

- `pohunek-sessiond` is a real per-session PTY owner. It owns the PTY master,
  child and process group, output drain, bounded history, terminal tracker,
  input ordering, resize, stop escalation, controller lease, and durable
  journal.
- `worker-protocol` provides the private versioned control and data protocol,
  bounded framing, output offsets, gap reporting, redacted launch payloads,
  one-use tokens, identity claims, and N/N-1 negotiation.
- the production daemon uses `WorkerLauncher` with the native systemd user
  manager. A subprocess launcher and real worker actor are available to test
  daemon behavior without restoring daemon-owned PTYs.
- daemon PTY ownership has been removed. `PtyHandle`, the daemon PTY reader,
  daemon output history, `portable_pty`, and every
  `RuntimeHandle::Legacy` path have been removed from the daemon. Launch
  construction is now a data-only `agent::LaunchCommand`.
- startup no longer performs automatic provider-native resume. It reconciles
  logical records with authenticated canonical worker sockets before serving
  clients.
- canonical worker discovery and `session.runtime_inventory` classify managed,
  orphaned, conflicting, incompatible, and identity-mismatched workers.
  Quarantined workers are left alive and release the attempted controller
  lease. The typed `session_runtime_discovered` event exposes discoveries.
- logical session records use crash-durable atomic replacement with file and
  parent-directory synchronization. Create, stop, remove, and recover
  transactions can be replayed or compensated after daemon failure.
- terminal journals are imported after a worker exits while the daemon is
  unavailable. Journal identity mismatch and duplicate evidence fail closed.
- `TerminalSnapshot`, `TerminalTracker`, and bounded OSC parsing live in
  `crates/terminal`; the worker no longer maintains a separate private VT
  model.
- launch identity is immutable, nested active identity is separate, and
  self-attach protection uses the stable worker ID instead of a daemon instance
  ID.
- explicit provider-native recovery preserves the logical session ID and
  `created_at`, creates a new worker/runtime/PID, commits a recover transaction,
  and emits `session_native_recovered`.
- public Rust and TypeScript protocol types, CLI, GUI, web client state,
  frontend lifecycle presentation, packaging, migration preflight, runbooks,
  architecture, public API, and assistant knowledge have been updated.
- session ID allocation advances past reconciled records and fails with
  `session_id_exhausted` instead of wrapping.
- ownership-proof worktree cleanup runs before a durable remove transaction is
  committed. Cleanup failure leaves the logical record available for retry.

### Focused validation

The following focused checks passed alongside the corresponding implementation
work and remain covered by the full suite:

- private protocol tests, worker tests, terminal tests, and daemon all-target
  checks;
- authenticated discovery tests for orphan, slot mismatch, duplicate claims,
  incompatible endpoints, symlinks, non-sockets, and unsafe permissions;
- create crash-window compensation and pre-commit worker adoption;
- terminal-journal import, identity mismatch/duplicate rejection, durable stop
  replay, and durable remove replay;
- subprocess-worker Unix and TCP lifecycle, attach, input, and stop tests;
- explicit recovery tests for lost and terminal runtimes, idempotency, logical
  identity preservation, and previous/new runtime event payloads;
- focused N/N-1 protocol negotiation and deterministic Codex/Claude identity
  fixture tests;
- packaging and migration-preflight tests.

### Resolved blocker

The expanded real-systemd scenario previously failed during the first daemon
adoption with `runtime state: conflict` / `loss reason:
runtime_identity_mismatch`. The root cause was a controller-lease leak in the
worker's control loop: when an event write on the control connection failed, the
lease was never released, so the replacement daemon's authenticated adoption was
rejected as an identity conflict. The fix releases the controller lease whenever
the control connection ends for any reason (EOF, `ReleaseController`, or a
write/serve error) and bumps the lease epoch, without relaxing discovery
authentication or quarantine rules. The scenario now runs to completion: both
workers classify as `Managed`, and worker PID, child PID, PTY device, and
runtime ID are preserved across daemon restart and `SIGKILL`, with explicit
native recovery creating a new runtime generation.

### Validation evidence

Every pre-merge item is complete with executable evidence on this branch:

1. `runtime_identity_mismatch` diagnosed and fixed (controller-lease release on
   any control-connection end) with no relaxation of discovery authentication or
   quarantine rules — see "Resolved blocker".
2. The real-systemd scenario
   (`daemon_restart_and_sigkill_preserve_systemd_worker_runtime`, gated by
   `POHUNEK_SYSTEMD_E2E=1`) passes end to end, retaining every continuity,
   output, operation, isolation, and recovery assertion.
3. The release/migration matrix is covered: mixed recoverable and unrecoverable
   legacy records (`import_legacy_manifest_mixed_recoverable_and_unrecoverable`),
   fail-closed manifest handling (fingerprint mismatch, live-loss,
   classification mismatch, unsupported schema, idempotent re-import), N/N-1
   negotiation (`negotiation_selects_highest_common_version`,
   `negotiation_rejects_disjoint_ranges`), archive-content
   (`release_workflow_packages_complete_daemon_runtime_set` plus the
   `cargo xtask docs check` release-extras check), and a live-runtime-preserving
   installer upgrade (`upgrade_refuses_before_overwrite_when_preflight_rejects_live_sessions`,
   `accepted_upgrade_records_authority_before_restarting_daemon`).
4. An end-to-end privacy test
   (`worker_backed_session_never_persists_secrets_or_terminal_bytes`) scans the
   logical store, worker journals, and structured logs for sentinel prompt,
   argv, environment, terminal-output, and hook values and permits only
   explicitly safe identifiers.
5. Public TypeScript and documentation artifacts are regenerated for the runtime
   inventory method/event; `cargo xtask ts check` and `cargo xtask docs check`
   pass and source maps resolve.
6. Every Section 26 gate passes on this branch: `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets --all-features` with `-D warnings`,
   `cargo test --workspace --all-features` (1,627 passed, 1 ignored — the
   systemd scenario, run separately under `POHUNEK_SYSTEMD_E2E=1`),
   `cargo build --workspace --release`, `cargo xtask ts check`, and
   `cargo xtask docs check`; plus web typecheck, lint, 99 unit tests, 13 browser
   end-to-end tests, and the three real-daemon web scenarios.

### External limitation

`cargo udeps` cannot run in this environment: it requires a nightly toolchain
(`-Z binary-dep-depinfo`), and no `rustup`/nightly toolchain is installed. This
external limitation is reported separately here; it does not conceal any other
skipped gate, all of which pass above. Run `cargo udeps --workspace
--all-targets` on a machine with a nightly toolchain to close this item.

## 1. Summary

This RFC changes the owner of every pohunek-managed PTY from the restartable
host daemon to a dedicated per-session process:

```text
local or remote client
        |
        | public pohunek protocol
        v
    pohunekd                         restartable control plane
        |
        | private local worker protocol
        v
pohunek-sessiond for s-42           durable runtime owner
        |
        | PTY master
        v
Codex, Claude Code, or shell         unchanged child process and PID
```

Each live logical session has exactly one `pohunek-sessiond` worker. The worker
owns the PTY master, child handle, PTY reader, raw-output history, and generic
terminal tracker. It runs in a systemd user service that is a sibling of
`pohunekd.service`, not a child process or lifecycle dependency of it.

Restarting, terminating, or killing `pohunekd` therefore closes client and
daemon-to-worker connections but does not close the PTY master or signal the
agent. The replacement daemon discovers the existing workers, reconnects to
them, restores its semantic session state, and resumes serving clients. The
agent process, process group, PTY, terminal contents, and native provider
conversation remain the same.

Provider-native resume remains available as an explicit recovery operation
after the worker or host has been lost. It is not used during an ordinary daemon
restart and is never presented as continuation of the same runtime.

This document is implementation-binding: the requirements expressed with
MUST, MUST NOT, SHOULD, and MAY are decisions, not unresolved questions. The RFC
defines the target behavior; the implementation-status section above records
which parts currently exist and which evidence is still missing. Released
behavior changes only when the implementation lands. The behavior and
knowledge documents named in Section 24 are updated with that implementation.

## 2. Motivation and Pre-implementation Failure

Before this implementation, `crates/daemon` owned all resources whose lifetime
was supposed to outlive control clients:

- `PtyHandle` owned the `MasterPty`, writer, child killer, blocking reader thread,
  exit watch, output broadcast, and raw-output ring.
- `SessionEntry` owned `PtyHandle`, detector and procwatch tasks, launch metadata,
  and active-agent claims.
- raw attach bridges called `PtyHandle` directly.
- daemon shutdown dropped the PTY handles, while the service manager also tore
  down descendants in the daemon service cgroup.
- startup compensated by loading a native resume binding and launching a new
  process.

That model supported detach from a CLI, but it did not support daemon
durability. A graceful daemon restart and a daemon crash both destroyed the live
runtime. Native resume created a different PTY and process, could recover only
sessions whose provider identity was captured, and could not recover shell
sessions or uncaptured agent sessions. It also lost transient terminal state
and could change provider behavior.

The product invariant is stronger:

> The lifecycle of a managed PTY and its child process is independent of the
> lifecycle of `pohunekd`.

Moving only the child process out of the daemon cgroup is insufficient. The
surviving process must retain an open PTY master, continue draining output, reap
the child, and accept future input and resize operations. A persistent runtime
owner is therefore required.

## 3. Relationship to the Existing Architecture

The hard constraints in `docs/architecture.md` remain unchanged:

- pohunek is single-operator and owner-private;
- there is no central server;
- each host is authoritative for its own PTYs and state;
- sessions use real PTYs and native agent TUIs;
- clients connect directly to the host daemon over the existing Unix or
  NetBird transport;
- the public control protocol remains newline-delimited JSON and public attach
  remains a separate raw byte stream;
- provider integrations remain outside the daemon's core runtime.

This RFC refines the meaning of "host daemon owns PTYs." The host-local pohunek
service remains authoritative, but PTY ownership is split between a restartable
control-plane process and isolated host-local runtime processes. Workers are not
remotely addressable, are not a new coordinator, and expose no public API.

When implementation lands, `docs/architecture.md` must describe
`pohunek-sessiond` as the runtime owner and `pohunekd` as the logical-session
authority and public control plane.

## 4. Goals

The implementation MUST provide all of the following:

1. The same PTY and child PID survive `SIGTERM`, `SIGKILL`, a systemd restart,
   and a binary upgrade of `pohunekd`.
2. Output is drained while the daemon is absent, within a configured bounded
   memory budget.
3. A replacement daemon reconnects without launching a provider-native resume.
4. The pohunek session ID, creation time, project and worktree association,
   launch identity, metadata, and native recovery reference remain stable.
5. Failure of one worker cannot terminate or corrupt any other worker.
6. A daemon crash during session creation, stop, removal, or reconciliation has
   a deterministic recoverable outcome.
7. Attach, input, resize, detector, procwatch, hook identity, and event behavior
   work after daemon reconnection.
8. A nested agent of the same provider cannot overwrite the immutable launch
   native identity.
9. Running workers remain usable across a supported daemon upgrade and rollback.
10. The first migration from daemon-owned PTYs is explicit and cannot silently
    destroy live sessions.

## 5. Non-goals

The following are deliberately outside this RFC:

- preserving a live PTY across host reboot, user-manager shutdown, kernel crash,
  or machine power loss;
- moving a live PTY between hosts;
- keeping an existing client socket open while `pohunekd` is unavailable;
- multi-user authorization or protection from another process running as the
  same operator;
- persisting raw terminal output or a rendered terminal screen to disk;
- making notification delivery durable while the daemon is absent;
- automatically restarting a crashed worker and pretending its lost PTY still
  exists;
- changing the NetBird or public attach trust model.

Host reboot and worker loss may be recoverable through an explicit
provider-native recovery, but that creates a new runtime generation.

## 6. Terms and Identities

### 6.1 Logical session

A logical session is the durable pohunek record identified by `SessionId`, for
example `s-42`. Its human metadata, project binding, worktree binding, launch
profile snapshot, creation time, and native recovery reference outlive any one
runtime.

### 6.2 Runtime generation

A runtime generation is one allocation of a PTY plus its root child. It has an
opaque random `runtime_id`. Reconnecting a daemon does not change the
`runtime_id`. Explicit native recovery creates a new `runtime_id`.

### 6.3 Worker

A worker is one `pohunek-sessiond` process and systemd unit activation. It owns
exactly one runtime generation and has an opaque random `worker_id`, generated
at worker startup. In the initial implementation `worker_id` and `runtime_id`
are equal after successful PTY spawn. They remain separate fields so a future
worker-internal pre-spawn lifecycle does not overload runtime identity.

### 6.4 Daemon instance

Each `pohunekd` process has a random `daemon_instance_id`. It identifies a
controller connection and log origin. It is never used to identify the PTY that
originated a client command.

### 6.5 Worker origin

Every child in a managed PTY inherits the stable `POHUNEK_WORKER_ID` and
`POHUNEK_SESSION_ID`. Public attach requests made from inside a managed PTY
carry both values. The self-feedback guard compares them to the target
session's current worker identity. This remains valid across daemon restarts.

### 6.6 Launch identity and active identity

The launch identity is the configured agent profile and base kind that created
the logical session. Its native provider identity is write-once for that
logical session.

The active identity describes an agent currently running inside the PTY. It may
be the launch agent or a nested agent, changes over time, and has its own native
ID, path, PID, process start time, and expiry.

## 7. Required Invariants

The implementation MUST maintain these invariants:

1. Only the worker holds the PTY master and portable-pty child handle.
2. `pohunekd` never receives, inherits, or reconstructs the PTY file descriptor.
3. Daemon shutdown never sends a signal to a worker or its child.
4. A normal child termination, an explicit `session.stop`, worker loss, and
   daemon disconnection are distinct outcomes.
5. At most one systemd worker unit may be active for a logical session.
6. At most one daemon controller holds the worker lease.
7. The daemon metadata store is authoritative for logical intent and metadata.
8. The live worker and its journal are authoritative for PTY/runtime facts.
9. Neither side may infer that the other side's missing record means it is safe
   to kill a process.
10. Reconciliation never automatically kills an ambiguous live worker.
11. Raw output, prompts, profile environment variables, and attach input are
    never persisted by the worker.
12. A launch native reference can be filled once but cannot be replaced by a
    hook from another PID, including a nested same-provider process.
13. Native resume is never invoked solely because the daemon disconnected.
14. Public client operations are served only after startup reconciliation has
    classified every known logical session and discovered worker.

## 8. Process and systemd Model

### 8.1 Installed units

The release installs:

- `pohunekd.service`;
- `pohunek-session@.service`;
- `pohunek-sessions.slice`.

`pohunek-session@.service` is instantiated by the safe one-component session ID:

```text
pohunek-session@s-42.service
```

The template has these semantic properties:

```ini
[Unit]
Description=Pohunek runtime worker for session %i

[Service]
Type=notify
NotifyAccess=main
ExecStart=%h/.local/libexec/pohunek-sessiond --session-id %i
Restart=no
KillMode=control-group
SendSIGHUP=yes
Slice=pohunek-sessions.slice

[Install]
WantedBy=
```

The final installed path may be selected by the installer, but it MUST be an
absolute path substituted into the unit. The unit MUST NOT use `PartOf=`,
`BindsTo=`, `Requires=`, `Requisite=`, or `WantedBy=pohunekd.service`.
`pohunekd.service` MUST NOT use `PropagatesStopTo=` or place workers in its own
service cgroup. A slice groups resources but does not create stop propagation
from the daemon.

`Restart=no` is mandatory. If the worker exits unexpectedly, the PTY master is
gone. Starting a replacement process cannot recreate it and would conceal
runtime loss.

### 8.2 Starting a worker without deadlock

`pohunekd` starts the instance through the systemd user manager D-Bus API. It
MUST NOT shell out to a blocking `systemctl start` from the creation path.

The sequence is:

1. daemon calls the user manager's `StartUnit` and receives the job object path;
2. worker creates its private runtime directory and binds its control socket;
3. worker sends `READY=1` to systemd immediately after the bootstrap socket is
   accepting connections, before waiting for `Initialize`;
4. daemon concurrently waits for the socket and monitors the D-Bus job;
5. daemon connects and sends `Initialize`;
6. worker allocates the PTY and launches the child.

Declaring readiness before `Initialize` prevents a `Type=notify` cycle in which
systemd waits for readiness while the daemon waits for the start job and the
worker waits for the daemon. `READY=1` means "bootstrap endpoint ready," not
"agent running." Agent readiness is reported by the private protocol.

The worker has a configured initialization deadline. If no valid controller
initializes it before that deadline, it writes a `never_initialized` outcome and
exits. The daemon monitors both the socket and unit job, so an early worker
failure returns a typed creation error.

### 8.3 Daemon readiness

`pohunekd.service` becomes `Type=notify`. The daemon sends `READY=1` only after:

- path and configuration validation;
- instance-lock acquisition;
- logical-store load;
- worker discovery and reconciliation;
- public Unix socket bind;
- required event-log initialization.

The optional NetBird listener may still degrade to local-only according to the
existing policy. Reconciliation errors that make individual sessions
unavailable do not prevent daemon readiness once they have been classified and
surfaced.

### 8.4 Explicit stop

`session.stop` asks the worker to terminate the PTY process group. The worker:

1. records an in-memory stop command ID;
2. sends `SIGTERM` to the managed process group through the retained child/process
   identity;
3. waits for the configured grace period;
4. sends `SIGKILL` only if the same retained process identity is still live;
5. reaps the root child;
6. records the terminal outcome atomically;
7. reports completion to the daemon.

Stopping the systemd worker unit is a last-resort administrative operation, not
the ordinary `session.stop` implementation. `KillMode=control-group` ensures
that an administrative unit stop or unexpected worker failure does not leave
unmanaged descendants.

### 8.5 Development and tests

Worker launch is represented by a `WorkerLauncher` trait. Production Linux uses
the systemd D-Bus implementation. Integration tests may use a subprocess
launcher that starts the worker in a separate process group and explicitly does
not parent its lifetime to the test daemon. Production startup fails clearly if
the worker template or user D-Bus is unavailable; it does not silently fall
back to daemon-owned PTYs.

### 8.6 Runtime configuration

Worker policy lives in dedicated typed `RuntimeConfig` and `WorkerConfig`
modules. Production defaults are named constants with documented rationales;
tests override the typed values. A worker rejects missing or invalid required
initialization policy rather than inventing values.

The initial defaults preserve current behavior where it already exists and
bound every new queue:

| Setting | Initial default | Rationale |
|---------|-----------------|-----------|
| worker bootstrap/initialize deadline | 30 seconds | allows a loaded user manager to start while bounding an abandoned unit |
| daemon worker-connect deadline | 10 seconds | matches an interactive create operation without hiding a broken unit |
| raw output history | 10,000,000 bytes | preserves the current per-session history budget |
| one subscriber queue | 1,000,000 bytes | absorbs repaint bursts without allowing one client to consume the history budget |
| worker data payload | 64 KiB | bounds allocation while efficiently carrying PTY and prompt fragments |
| worker JSON header/control line | 64 KiB | private control metadata never requires prompt-sized allocation |
| one-use data token lifetime | 10 seconds | preserves the current attach-token window |
| completed input dedup entries | 4,096 | covers practical retries while bounding memory for a long-lived worker |
| stop grace | 500 milliseconds | preserves current explicit-stop behavior |
| terminal worker retention | 24 hours | permits a prolonged daemon outage without disk-persisting the final screen |
| reconnect backoff | 100 milliseconds to 5 seconds | avoids busy loops while recovering promptly |
| active identity claim expiry | 30 seconds | preserves current unverified-claim policy |

Byte, duration, and count values are validated for nonzero safe ranges. Output
history, subscriber queues, and dedup entries also have documented hard upper
bounds to prevent a local configuration mistake from exhausting the host.
Configuration values that affect an initialized runtime are frozen in that
worker; editing host configuration affects only later runtime generations.

## 9. Code and Ownership Split

The workspace gains:

| Component | Responsibility |
|-----------|----------------|
| `crates/worker-protocol` | private versioned types, framing, negotiation, and redacting request types |
| `crates/session-worker` | worker library, PTY actor, output ring, terminal tracker, journal, hook endpoint |
| `pohunek-sessiond` binary | path resolution, systemd notification, one worker event loop |
| `crates/daemon::runtime` | launcher, worker client, controller lease, discovery, and reconciliation |

`worker-protocol` and the `session-worker` library use
`#![forbid(unsafe_code)]`. Any required Linux peer-credential or signal primitive
must live behind a small reviewed platform module with typed errors and the
workspace's localized unsafe policy.

The existing `PtyHandle`, blocking reader, output ring, exit watcher, and generic
terminal tracking move from `crates/daemon` into `crates/session-worker`.
Provider-independent terminal interpretation belongs in `crates/terminal`.

The daemon retains:

- public Unix and NetBird servers;
- attach token and public raw-stream policy;
- logical session, project, worktree, and metadata authority;
- provider profile resolution and structural launch planning;
- semantic activity detector and provider manifests;
- procwatch interpretation;
- notifications, event log, and client-facing errors;
- explicit native recovery orchestration.

`SessionEntry` no longer contains a PTY handle. It contains a `WorkerClient`,
the current `runtime_id`, reconstructed detector/procwatch handles, logical
session data, and attach bookkeeping. Every direct call from the public API to
`PtyHandle` is replaced by a worker operation.

## 10. Filesystem Contract and Permissions

`crates/paths` becomes the single source of these paths:

```text
$XDG_RUNTIME_DIR/pohunek/
  daemon.sock
  daemon.lock
  workers/
    s-42/
      control.sock

$XDG_STATE_HOME/pohunek/
  logs/
  workers/
    s-42/
      <worker-id>.json

$XDG_DATA_HOME/pohunek/
  metadata.jsonl
  worktrees/
  events/
```

Modes are:

- every pohunek runtime, state, and worker directory: `0700`;
- daemon and worker Unix sockets: `0600`;
- worker journals and logical-store files: `0600`;
- atomic temporary files: created with `0600`, never created permissively and
  chmodded later.

Session IDs and worker IDs pass `valid_runtime_id` and an additional ASCII
allowlist before being used as path components or systemd instance names.
Symlinks are rejected for worker directories, sockets, journals, and atomic
temporary targets. Files are opened relative to a verified owner-private
directory where the platform permits it.

The runtime socket is fixed by logical session ID, while the handshake returns
the random worker ID. A worker may remove a stale socket only when it owns the
unit activation, connection to the path fails, and the path is an owner-owned
Unix socket. A daemon never unlinks a worker socket merely because a connection
attempt failed.

### 10.1 Worker journal

The worker is the sole writer of its journal. The journal contains:

- journal schema version;
- session ID, worker ID, runtime ID, worker protocol range;
- worker PID and process start identity;
- root child PID, process group identity, and process start identity;
- PTY creation timestamp and current terminal dimensions;
- runtime phase and terminal outcome;
- immutable launch native reference, when captured;
- latest sanitized active-agent identity claim;
- last output offset, but no output bytes;
- whether terminal state was acknowledged by a daemon.

It contains no command environment, prompt, input bytes, terminal bytes,
rendered screen, attach token, controller token, or provider notification body.

Every phase transition is written as a same-directory temporary file, flushed,
atomically renamed, and followed by a directory sync. Failure to durably record
the initial live phase after child spawn is fatal to that creation: the worker
terminates the managed process and returns a typed error instead of running an
unreconcilable PTY. Failure to record a later terminal phase is logged and
reported; the worker retains the terminal result in memory for daemon
reconciliation.

### 10.2 Logical store

The daemon is the sole writer of logical session records. A new tagged record
kind replaces `ResumeBinding` as the session authority:

```text
SessionRecord
  schema_version
  session_id
  created_at
  updated_at
  desired_state
  transaction
  launch
  native_recovery
  runtime_binding
  project_and_worktree
  metadata
  terminal_summary
```

`launch` stores the existing structural snapshot: agent name and base, program,
arguments, input rules, resume mode and reference kind, cwd, and initial
dimensions. It never stores profile environment variables or initial input.

`runtime_binding` contains worker ID, runtime ID, unit name, and last observed
runtime phase. It does not claim authority over PID or exit facts; those are
validated against the live worker or journal.

`desired_state` is one of `running`, `stopped`, or `removed`. It makes stop and
remove intent recoverable across daemon failure.

`transaction` records the current idempotent create, stop, recover, or remove
operation ID and phase. Store rewrites use the same durable atomic-write rules
as the worker journal. Worktree and session record mutations that form one
logical transition are serialized by the store and written in one replacement.

## 11. Private Daemon-Worker Protocol

The worker protocol is local-only. It is never exposed over NetBird and is not
part of `crates/protocol` or the public compatibility promise.

### 11.1 Transport and peer checks

All worker connections use the owner-private Unix socket. Both sides verify
peer UID with Unix peer credentials. A worker accepts only its own effective
UID. The daemon verifies the worker PID against the systemd unit's `MainPID` and
then verifies `worker_id`, session ID, and process start identity from the
handshake and journal. PID equality without start identity is insufficient.

The socket supports:

- one leased control connection;
- zero or more framed data connections opened by the leased controller;
- short worker-local identity-hook connections from descendants.

### 11.2 Version negotiation

The first control request is:

```json
{
  "type": "negotiate",
  "request_id": "…",
  "daemon_instance_id": "…",
  "minimum_version": 1,
  "maximum_version": 2
}
```

The worker replies with its supported range, selected version, session ID,
worker ID, runtime ID when initialized, worker PID and start identity, runtime
phase, capabilities, and a random connection-bound lease challenge.

The selected version is the highest common version. No common version returns
`worker_protocol_incompatible` and leaves the worker and child untouched.
Unknown additive JSON fields are ignored. Unknown frame kinds or semantic
commands fail the affected request without closing the PTY.

Each worker-aware release supports its current worker protocol and the preceding
release's protocol. Section 20 defines the release-boundary exception for the
first worker-aware release.

### 11.3 Controller lease

After negotiation the daemon sends `AcquireController` with:

- daemon instance ID;
- lease challenge response;
- requested capabilities.

The worker grants one random, memory-only `lease_id`. The lease is bound to the
control connection, peer PID/start identity, and daemon instance ID. It is
released immediately on control-connection EOF or an explicit
`ReleaseController`.

There is no time-based forced lease stealing. A paused but connected daemon
still owns the daemon instance lock, so a second valid control plane cannot
start normally. If a conflicting controller appears, the worker returns
`controller_busy`; it does not terminate either process or the PTY.

Every mutating request carries `lease_id`, `request_id`, session ID, worker ID,
and runtime ID. A mismatch is rejected before any PTY operation.

### 11.4 Control framing

Control and hook connections use size-bounded newline-delimited JSON. The
maximum line size is a named configuration constant and malformed or oversized
lines close only that connection. Request/response pairs carry `request_id`.

Control operations are:

| Operation | Meaning |
|-----------|---------|
| `Inspect` | return runtime, process, terminal, identity, and offset snapshot |
| `Initialize` | supply the one-shot launch plan to an uninitialized worker |
| `OpenDataStream` | mint a one-use token for a framed data connection |
| `WritePlan` | execute deduplicated ordered input fragments |
| `Resize` | idempotently resize the PTY and terminal tracker |
| `Stop` | idempotently terminate the retained process group |
| `AcknowledgeTerminal` | confirm daemon imported final state |
| `ReleaseController` | gracefully release the lease |

Worker events on the control connection are:

- `RuntimeStarted`;
- `OutputAdvanced`;
- `TerminalChanged`;
- `IdentityChanged`;
- `ChildExited`;
- `RuntimeFault`.

Events include a monotonically increasing worker event sequence. After
reconnection the daemon calls `Inspect`; it never assumes it received events
while disconnected.

### 11.5 Initialization

`Initialize` is valid exactly once and carries:

- session and transaction IDs;
- expected worker ID from the completed negotiation;
- sanitized logical launch identity;
- resolved executable, arguments, cwd, dimensions, and input rules;
- environment variables in a redacting type;
- output and subscriber memory limits;
- stop and terminal-retention policy;
- worker-hook protocol version.

The environment exists only in the request buffer and child construction. The
worker request type has a handwritten redacted `Debug`, request logging excludes
the value, and buffers are dropped immediately after spawn. Initialization is
idempotent by transaction ID: repeating the same ID returns the recorded result;
a different ID is rejected.

Before spawn, the worker removes its own `NOTIFY_SOCKET`, watchdog, controller,
and bootstrap variables from the child environment, then appends the reserved
`POHUNEK_*` session values after profile environment. A profile cannot override
reserved values. `NotifyAccess=main` and the sanitized environment prevent the
PTY child from participating in worker service readiness.

### 11.6 Data framing

Daemon-worker output and raw attach input use a binary-safe framed connection,
not JSON base64. Each frame is:

```text
4-byte big-endian JSON-header length
JSON header bytes
4-byte big-endian payload length
payload bytes
```

Header and payload sizes are validated before allocation against named limits.
The header contains the selected version, frame kind, stream ID, runtime ID, and
kind-specific fields. Payload is empty for metadata-only frames.

Required frame kinds are:

- `Open`;
- `Replay`;
- `Output`;
- `TerminalSnapshot`;
- `Gap`;
- `Input`;
- `InputAck`;
- `Exit`;
- `Error`;
- `Close`.

The data-stream token is random, one-use, short-lived, tied to the current lease
and runtime, and never persisted or logged.

## 12. Output Continuity and Terminal Snapshot

The worker reader drains the PTY continuously, even with no daemon or attached
client. It owns:

- a bounded raw-byte ring;
- monotonically increasing byte offsets;
- a bounded per-subscriber queue;
- a provider-independent VT tracker;
- current title, OSC progress, dimensions, cursor, alternate-screen state, and
  visible screen.

Offsets are unsigned 64-bit byte positions within one runtime generation:

- `history_start_offset` is the first retained byte;
- `next_offset` is the offset immediately after the last observed byte;
- each output frame covers `[offset, offset + payload_length)`.

Offset overflow is treated as a worker runtime fault and never wraps.

### 12.1 Atomic snapshot and subscribe

Opening an output stream is one operation in the worker actor:

1. validate and redeem the data token;
2. register the subscriber;
3. capture `history_start_offset`, `next_offset`, and a terminal snapshot at the
   same actor turn;
4. select replay or gap behavior;
5. release the actor to continue reading PTY output.

No PTY byte can exist between snapshot and subscription without being either in
the captured replay range or queued as live output.

If the requested `after_offset` is within the retained ring, the worker sends
`Replay` frames beginning exactly at that offset, then `Output` frames beginning
at the captured `next_offset`.

If no offset is supplied, a new raw attach receives the retained ring followed
by live output. A daemon detector reconnection requests its last processed
offset.

If the requested offset precedes `history_start_offset`, the worker sends:

1. a `Gap` with the missing range;
2. a `TerminalSnapshot` representing the complete current display and parser
   signals at a specific watermark;
3. live `Output` beginning at that watermark.

The terminal snapshot is a structured private-protocol value for daemon state
rehydration plus an ANSI-rendered payload suitable for public raw attach
repainting. It includes enough state to reconstruct the current screen without
replaying an escape sequence whose prefix was evicted from the ring.

If a subscriber queue overflows while replay is being delivered, the worker
discards that subscriber's pending output, emits a new `Gap` and current
`TerminalSnapshot`, and resumes at the new watermark. It never silently skips
bytes. A repeatedly slow public client is disconnected with a typed reason
rather than consuming unbounded worker memory.

### 12.2 Daemon detector recovery

The daemon records only the last processed output offset in memory. On
reconnection it obtains the worker's terminal snapshot before enabling semantic
events. It reconstructs provider detector inputs from visible screen, title,
progress, dimensions, process facts, and subsequent output.

A gap resets incremental detector evidence that cannot be reconstructed, while
the full terminal snapshot remains authoritative for current visible state.
The daemon emits state changes only after rehydration; it does not emit a
spurious `done` or `failed` during its own outage.

### 12.3 Public attach behavior

An existing public Unix or TCP attach connection ends when `pohunekd` exits.
This is not a session exit. CLI, GUI, and web clients reconnect to the new
daemon and attach to the same logical session and runtime ID.

The daemon translates private frames to the existing raw public byte stream. It
uses the ANSI terminal snapshot on a gap and never exposes private headers.
Client-side automatic reconnect must compare runtime ID: the same runtime may
be repainted; a changed runtime is presented as explicit recovery, not seamless
continuation.

## 13. Input and Resize Semantics

### 13.1 Deduplicated input plans

Every daemon-generated input operation has a `write_id` unique within the
runtime. For public `session.input`, it is derived from the runtime ID and
public request ID so retrying the same request produces the same value.

`WritePlan` contains ordered byte fragments and configured delays. The worker,
not the daemon, performs delayed TUI framing such as bracketed paste and a later
submit byte. Therefore a daemon crash between fragments does not leave a new
daemon guessing which fragments were written.

The worker keeps a bounded map of in-progress and completed write IDs:

- first receipt starts the plan;
- a duplicate while in progress joins the same result;
- a duplicate after completion returns the prior acknowledgement;
- reuse of a write ID with different content is rejected;
- completion is acknowledged only after every fragment is written and flushed.

The dedup map lives for the worker lifetime and therefore survives daemon
restart. Its capacity and retention are configured and bounded. Eviction never
causes an automatic retry; an expired ambiguous write returns
`write_outcome_unknown`.

Raw attach input is chunked by the daemon into write plans with stream-scoped
monotonic IDs. The daemon does not retry a raw chunk after losing its worker
connection. This gives at-most-once retry behavior: a final chunk may be lost
during simultaneous connection failure, but it is never duplicated by
reconnection.

### 13.2 Resize

Resize requests carry a runtime ID, attachment or control source ID, and
monotonic source sequence. Duplicate or older source sequences are ignored.
The daemon retains the public last-attach-wins policy and sends the resulting
dimensions to the worker. `Inspect` returns actual PTY dimensions, allowing a
replacement daemon to restore its public snapshot.

## 14. Creation Transaction and Crash Windows

Creation is a durable transaction. The ordered phases are:

1. validate public parameters and resolve the agent profile;
2. allocate session ID and create a `SessionRecord` with
   `desired_state=running`, `transaction=create/preparing`, and structural
   launch metadata;
3. resolve project and create or bind the worktree, atomically updating the
   session and worktree records;
4. ask systemd to start `pohunek-session@<session>.service`;
5. negotiate with the bootstrap worker and record worker ID;
6. send the one-shot `Initialize`, including secret environment only in memory;
7. worker allocates PTY, spawns the child, writes its live journal, and returns
   root process identity and runtime ID;
8. daemon validates the journal/response and commits
   `transaction=none`, `runtime_binding=live`;
9. daemon starts detector and procwatch, emits `session_created`, runs session
   hooks, and delivers any initial input through a deduplicated `WritePlan`.

The public create response is successful only after phase 8 and successful
initial input. `session_created` is emitted exactly once.

### 14.1 Crash outcomes

| Crash point | Reconciliation outcome |
|-------------|------------------------|
| before phase 2 | no session exists |
| after phase 2, before worktree bind | mark create failed and remove empty record |
| during worktree bind | inspect authoritative worktree binding; finish or compensate without guessing |
| after unit start, before initialization | worker initialization deadline exits it; mark create failed |
| during `Initialize`, before child spawn | same transaction ID is retried or worker reports pre-spawn failure |
| after child spawn, before live journal | worker terminates child because it cannot establish recoverable authority |
| after live journal, before daemon commit | daemon adopts worker into the preparing logical record and commits |
| after commit, before event | event log reconciliation emits one recovered creation event keyed by transaction ID |
| during initial input | same write ID is inspected/retried; definite failure triggers durable stop and worktree compensation |

Compensation never kills a live worker whose identity does not exactly match the
preparing record. Such a mismatch becomes `runtime_conflict` for operator
inspection.

## 15. Startup Discovery and Reconciliation

The daemon reconciles before advertising readiness:

1. acquire the daemon instance lock;
2. load and validate all logical records;
3. enumerate active `pohunek-session@*.service` units through user D-Bus;
4. scan owner-private worker journals and runtime sockets;
5. connect, negotiate, validate peer identity, and acquire controller leases;
6. call `Inspect` on every connected worker;
7. classify every logical record and every discovered worker;
8. finish recoverable transactions;
9. construct `SessionEntry` worker proxies and restart detector/procwatch tasks;
10. bind the public socket, notify readiness, and emit reconciliation events.

Reconciliation uses this table:

| Logical record | Worker/journal | Result |
|----------------|----------------|--------|
| running, exact live identity | live | reconnect and mark live |
| preparing, exact live identity | live | adopt and commit creation |
| running, terminal journal | no live worker | import terminal outcome |
| stop requested, live | live | replay same idempotent stop command |
| remove requested, live | live | finish stop, then removal |
| running, no runtime evidence | absent | mark `runtime_lost`; do not native-resume |
| terminal | stale inactive journal | keep summary, schedule safe journal cleanup |
| no logical record | one valid live worker | create quarantined recovered record and expose `orphaned_worker`; do not kill |
| any | incompatible live worker | expose `worker_protocol_incompatible`; leave it alive |
| any | multiple live identities | expose `runtime_conflict`; do not acquire mutation authority or kill |
| worker identity mismatch | any | expose `runtime_identity_mismatch`; fail closed |

An orphaned live worker can occur only if the logical store was lost after the
worker journal became durable. The recovered record contains journal-safe
metadata and is attachable only after identity validation. Operations that need
missing project or launch metadata fail with precise errors. The operator may
rename, stop, remove, or explicitly adopt it; reconciliation does not invent
missing metadata.

Existing sessions do not emit `session_created` on daemon startup. The event log
receives `session_runtime_reconnected`, `session_runtime_lost`,
`session_runtime_conflict`, or `session_native_recovered` with logical and
runtime identities.

## 16. Child Exit, Terminal Retention, Stop, and Removal

### 16.1 Natural exit

When the child exits, the worker drains PTY EOF, reaps the child, finalizes the
terminal tracker, and atomically records exit code, signal, success, and
timestamp. It enters terminal-retention mode.

While retained, the worker serves final history and terminal snapshot. Once a
daemon imports and durably commits the outcome, it sends
`AcknowledgeTerminal`; the worker marks the acknowledgement and exits. If the
daemon remains absent, the worker retains state for the configured terminal
retention period, then exits with the journal still present. Raw output and the
rendered screen disappear when the process exits and are never written to disk.

### 16.2 Durable stop intent

Before sending `Stop`, the daemon commits `desired_state=stopped` and the stop
transaction ID. If it crashes before delivery, reconciliation delivers the same
stop. If it crashes after delivery, the worker deduplicates the command and the
journal supplies the outcome.

Daemon shutdown itself never writes stop intent.

### 16.3 Durable remove intent

Removing a live session first commits `desired_state=removed`, then completes
the idempotent stop, imports the terminal result, removes owned worktree state
according to existing safety rules, removes the systemd unit's inactive state
and worker journal, and finally deletes the logical record.

If removal crashes, reconciliation continues from the recorded phase. Files or
worktrees are never deleted merely because a worker cannot be contacted.

### 16.4 Cleanup

Cleanup checks session ID, worker ID, runtime ID, file ownership, file type, and
unit inactivity. It removes only the exact runtime socket directory and journal
named by the completed logical transaction. It never uses an unresolved glob,
symlink target, broad XDG directory, or PID alone.

## 17. Provider Identity and Hooks

### 17.1 Stable hook endpoint

Managed session state hooks receive:

- `POHUNEK_ENV=1`;
- `POHUNEK_SESSION_ID`;
- `POHUNEK_WORKER_ID`;
- `POHUNEK_WORKER_SOCKET_PATH`;
- worker-hook protocol version;
- the stable daemon socket path for existing notification delivery.

Identity hooks prefer the worker socket. The worker validates owner UID and
that the reported agent PID and process start identity are descendants of the
retained PTY root. It stores the latest sanitized identity state and forwards it
to the connected daemon. Identity reports made while the daemon is absent are
therefore visible after reconnection.

Notification hooks continue to target the daemon. Missing notifications during
a daemon outage are permitted by the non-goals and do not affect identity or
runtime continuity.

### 17.2 Launch process designation

For a Codex or Claude launch profile, the worker designates one immutable launch
agent process identity:

1. if the root child is the configured provider process, use its PID and process
   start identity;
2. if a configured wrapper spawns or execs the provider, procwatch selects the
   earliest matching descendant in the launch lineage;
3. a hook claim received before designation is held briefly and validated after
   process observation;
4. ambiguity rejects launch-native binding and produces a diagnostic; it does
   not guess.

Shell sessions have no launch provider process. Every agent started inside a
shell is active/nested identity only.

### 17.3 Immutable native recovery reference

A native ID or transcript path may populate `launch_native` only when:

- the hook provider base equals the launch base;
- PID plus process start identity equals the designated launch process;
- the reference kind matches the frozen launch resume template;
- the value passes existing validation;
- `launch_native` is empty or already equal to the same value.

A conflicting second value is rejected and logged without including sensitive
content. A nested process, including nested Codex inside a Codex session, may
update `active_native` but cannot update `launch_native`.

The worker journals an accepted launch reference before acknowledging the hook.
The daemon then persists it into `native_recovery`. Reconciliation copies a
journaled value only after worker identity validation.

### 17.4 Active identity

Active identity is keyed by provider, PID, and process start identity. Claims
retain existing sequence and expiry semantics. Procwatch validates liveness;
process exit or release clears the active fields. The launch fields never
change when active fields change.

### 17.5 Self-attach protection

The public client reads `POHUNEK_SESSION_ID` and `POHUNEK_WORKER_ID` from its
environment and sends them as attach origin. The daemon rejects attach when both
match the target session's current runtime.

`POHUNEK_DAEMON_ID` is removed from the self-feedback decision. A PTY created by
an earlier daemon retains the same worker marker, so a replacement daemon cannot
accidentally allow an output-to-input loop.

## 18. Explicit Native Recovery

`session.resume` remains an explicit user operation and is extended to a
logical session whose runtime is terminal or `runtime_lost`. It is rejected for
a live, connecting, conflicting, or merely daemon-disconnected runtime.

Recovery:

1. validates the persisted structural launch snapshot and native reference;
2. records a recover transaction and new desired runtime generation;
3. starts a new worker through the ordinary creation protocol;
4. launches the provider's native resume command;
5. preserves logical session ID, creation time, metadata, project, and
   worktree;
6. assigns new worker and runtime IDs and a new child PID;
7. emits `session_native_recovered` with old and new runtime IDs.

Clients must visibly distinguish recovery from reconnection. Recovery is never
automatic on daemon startup, worker incompatibility, worker connection timeout,
or ambiguous reconciliation.

## 19. Public Session Model

The public `SessionInfo` gains an additive `runtime` object:

```text
runtime.state =
  starting | live | reconnecting | terminal | lost | conflict | incompatible
runtime.worker_id
runtime.runtime_id
runtime.started_at
runtime.last_connected_at
runtime.loss_reason
```

The existing `state` and `activity` remain agent/process semantics. During the
short startup barrier clients cannot query partially reconciled sessions.
After readiness, an unavailable runtime has an explicit runtime state rather
than being omitted or silently shown as a new process.

Attach, input, and resize return typed errors for `lost`, `conflict`, and
`incompatible`. List and inspect always retain the logical record. TypeScript
bindings, SDKs, CLI output, GUI core, web control center, public API docs, and
assistant knowledge are updated together when this field lands.

## 20. Upgrade, Rollback, and Packaging

### 20.1 Release contents

The daemon release archive contains:

- `pohunekd`;
- `pohunek-sessiond`;
- `pohunekd.service`;
- `pohunek-session@.service`;
- `pohunek-sessions.slice`;
- an installer/uninstaller that substitutes absolute binary paths, reloads the
  user manager, verifies unit definitions, and preserves running workers.

Both glibc and MUSL daemon archives contain both binaries. CI builds and tests
both binaries for each daemon target. Updating the on-disk worker binary does
not affect an already mapped worker process.

The installer restarts only `pohunekd.service`. It MUST NOT restart, stop,
`try-restart`, or daemon-reload through a target that propagates to worker
units. Uninstallation refuses while workers are live unless the operator
explicitly requests a destructive stop and receives the list of affected
sessions.

### 20.2 Worker-aware N/N-1 compatibility

Every worker-aware daemon supports worker protocols from its own release and
the immediately preceding worker-aware release. Every worker binary offers the
same two-version range and negotiates down. Therefore:

- a new daemon connects to workers started by the prior release;
- after rollback, the prior daemon connects to workers started by the newer
  release using the prior protocol;
- unsupported new capability is disabled for that connection, not emulated;
- an incompatible worker is left alive and exposed as incompatible.

Logical-store schema changes during this window are additive. Writers retain
fields required by the preceding worker-aware release, and readers ignore
unknown fields. Migration tests cover both upgrade and rollback fixtures.

### 20.3 First worker-aware release boundary

The legacy daemon cannot adopt workers and must not be treated as N-1 compatible
with the first worker-aware release. New session records use a new tagged record
kind, not the legacy `resume` kind. If a legacy binary is manually started
against the migrated store, it ignores the new records rather than
native-resuming a second agent beside a live worker.

Supported rollback across this boundary requires:

1. enumerate and explicitly stop or finish every worker runtime;
2. export recoverable sessions to legacy resume records;
3. remove or disable worker units;
4. start the legacy daemon;
5. explicitly native-resume exported sessions.

The installer refuses a boundary rollback while a worker unit is live.

## 21. One-time Migration from Daemon-owned PTYs

An existing PTY master cannot be transferred after the legacy daemon has
exited. Linux descriptor passing requires both processes to cooperate while
alive, and the legacy daemon has no handoff protocol. The first migration is
therefore an explicit compatibility boundary; all subsequent daemon restarts
are lossless.

The new CLI and installer provide a migration preflight that runs while the
legacy daemon is still alive:

1. query and snapshot every visible logical session through the public
   protocol;
2. merge available legacy native resume and worktree records;
3. write an owner-private, sanitized migration manifest atomically;
4. classify each live session as recoverable by native reference or
   unrecoverable;
5. print the exact affected session IDs and refuse daemon replacement by
   default.

The normal migration path is to let live legacy sessions finish, stop them
explicitly, and rerun preflight with zero live sessions.

An operator may pass an explicit `--accept-runtime-loss` migration option. It
records informed consent in the manifest, stops the legacy daemon, imports every
snapshot as a logical record with `runtime=lost`, and does not launch anything.
Recoverable sessions can then be resumed explicitly. Uncaptured and shell
sessions remain visible as lost records but cannot be reconstructed.

Startup never treats a legacy `resume` record as proof that no live legacy
daemon exists. Migration requires the legacy daemon instance lock to be released
and the signed-off manifest to match the store fingerprint. An incomplete or
mismatched migration fails closed with an actionable diagnostic.

## 22. Security and Privacy

The design preserves the single-operator trust boundary while reducing
accidental exposure:

- all local endpoints and state are owner-private;
- worker endpoints verify peer UID;
- daemon validates worker PID and process start identity against systemd;
- hook PIDs are validated by ancestry and start identity;
- process signals use the retained child/process-group handle, pidfd where
  available, and start identity checks rather than an unverified numeric PID;
- worker and daemon protocol types carrying environment or input have redacted
  `Debug` implementations;
- structured logs never include environment values, prompt/input bytes,
  terminal bytes, data tokens, controller tokens, or native reference values;
- errors identify fields and session/worker IDs without echoing secret-bearing
  payloads;
- raw output and rendered screens stay in bounded memory;
- journal and metadata writes reject symlinks and use atomic owner-private
  files;
- cleanup validates exact typed identities and never follows broad paths;
- the private worker socket is never proxied to NetBird.

Profile environment is resolved by the daemon at creation or explicit recovery,
transmitted once over the owner-private socket, and discarded after child spawn.
It is not required for daemon reconnection because the original child remains
alive.

## 23. Observability

Daemon and worker logs are structured JSON in the existing state log
directory. Every lifecycle log includes session ID, worker ID, runtime ID,
operation ID where applicable, phase, and outcome. It excludes the payloads
listed in Section 22.

Required daemon metrics or structured counters are:

- reconciliation duration;
- logical sessions loaded;
- workers discovered, connected, adopted, lost, conflicting, and incompatible;
- controller reconnect attempts and latency;
- output gaps and dropped slow subscribers;
- deduplicated input requests and unknown input outcomes;
- create, stop, remove, and recovery transaction completion;
- worker protocol version pairs.

Required worker events are:

- bootstrap ready;
- initialization accepted or rejected;
- PTY allocated and child started;
- controller acquired, released, or rejected;
- output ring eviction and subscriber gap;
- hook identity accepted or rejected by reason;
- explicit stop stage;
- child exit and terminal acknowledgement;
- journal write failure;
- worker shutdown reason.

Daemon disconnection is logged as control-plane loss, never as child exit.

## 24. Implementation Sequence

The work is delivered in the following dependency order. Intermediate branches
may be incomplete, but the feature is not enabled in production until all
sections and migration safeguards are complete.

1. **Contracts and paths**
   - add worker and logical-runtime types;
   - add private protocol framing and version negotiation;
   - add XDG worker paths, modes, validation, and atomic journal primitives.
2. **Worker runtime**
   - move PTY ownership, reader, exit handling, output ring, and terminal tracker;
   - implement initialization, input dedup, resize, stop, hook identity, journal,
     and terminal retention;
   - add the `pohunek-sessiond` binary and subprocess integration harness.
3. **systemd supervision**
   - add D-Bus launcher and unit validation;
   - add template, slice, daemon notify readiness, packaging, and installer;
   - prove sibling-unit lifecycle independence.
4. **Daemon worker client**
   - replace `PtyHandle` in `SessionEntry`;
   - proxy attach, input, resize, inspect, and stop;
   - restore detector/procwatch from worker snapshots.
5. **Durable logical lifecycle**
   - add `SessionRecord`, transaction phases, two-phase create, durable stop and
     remove;
   - implement startup discovery, reconciliation, orphan quarantine, and
     terminal import.
6. **Identity and recovery**
   - switch identity hooks to worker origin;
   - enforce launch/native immutability and nested active identity;
   - implement explicit native recovery as a new runtime generation.
7. **Clients and public contract**
   - expose runtime state and reconnection events;
   - implement CLI, GUI, and web reconnect behavior and visible recovery
     distinction;
   - regenerate TypeScript types.
8. **Migration and compatibility**
   - implement legacy preflight/import, boundary rollback guard, N/N-1 fixtures,
     and release installation tests.
9. **Documentation and activation**
   - update `docs/architecture.md`, `docs/public-api.md`, README, runbooks,
     release guidance, and all affected `docs/knowledge/` sources and source map;
   - remove automatic startup `load_and_resume`;
   - enable worker-backed creation only after the full test matrix passes.

The old daemon-owned PTY path is removed when activation lands. There is no
long-lived dual implementation or silent fallback.

## 25. Failure Matrix

| Failure | Required behavior |
|---------|-------------------|
| client disconnect | worker and agent continue |
| daemon graceful shutdown | lease releases; worker and agent continue |
| daemon `SIGKILL` | kernel closes lease connection; worker and agent continue |
| systemd restart of daemon | sibling worker units remain active; new daemon reconnects |
| daemon binary upgrade | same PTY, worker, child PID, and runtime ID |
| supported daemon rollback | negotiate prior protocol; same runtime continues |
| daemon absent while output arrives | worker drains into bounded ring and terminal tracker |
| output ring overrun | explicit gap plus atomic terminal snapshot |
| daemon absent during identity hook | worker records identity; daemon imports on reconnect |
| daemon absent when child exits | worker journals outcome and retains final terminal in memory |
| worker crash or `SIGKILL` | PTY is lost; systemd does not restart worker; mark runtime lost |
| one worker crash | no other worker or daemon is terminated |
| child exits naturally | terminal outcome, no native auto-resume |
| host reboot or user-manager shutdown | runtime lost; optional explicit native recovery |
| create daemon crash | deterministic transaction reconciliation from Section 14 |
| stop daemon crash | replay same idempotent stop intent |
| remove daemon crash | continue recorded removal transaction safely |
| incompatible worker | leave alive, expose typed incompatible state |
| duplicate or mismatched worker | quarantine conflict; never automatically kill |
| corrupt logical record | isolate record, fail startup readiness only if safe classification is impossible |
| corrupt worker journal with live unit | inspect live worker; quarantine on identity ambiguity |
| systemd user D-Bus unavailable | existing public daemon may inspect metadata, but new runtime creation fails fast |
| missing worker unit or binary | creation fails before logical success and compensates safely |
| slow output subscriber | gap/snapshot recovery, then bounded disconnect |
| duplicate input request | same completion, no duplicate PTY bytes |
| same write ID with different bytes | reject without writing |
| nested same-provider hook | active identity may change; launch native identity cannot |
| first legacy migration with live PTYs | installer refuses unless explicit runtime-loss consent |

## 26. Test Strategy

### 26.1 Unit tests

`worker-protocol` tests cover:

- partial reads and writes for every framing boundary;
- oversized, malformed, unknown, and mismatched frames;
- version negotiation and N/N-1 ranges;
- redacted formatting for secret-bearing types;
- data token expiry and one-use redemption.

Worker tests cover:

- output offsets and ring eviction;
- atomic snapshot/subscribe under concurrent output;
- initial replay, gap snapshot, and subscriber overflow;
- VT snapshot after fragmented escape and OSC sequences;
- write-plan ordering, delay ownership, duplicate join, completed duplicate,
  conflicting duplicate, and dedup eviction;
- resize sequence ordering;
- initialize idempotency and deadline;
- stop signal escalation and retained process identity;
- terminal journal and acknowledgement;
- launch identity selection, hook ancestry, nested same-provider rejection, and
  active identity expiry;
- journal permissions, atomicity, corruption, and secret absence.

Daemon tests cover:

- every logical transaction transition;
- store/worker authority rules;
- every reconciliation row in Section 15;
- stable worker-origin self-attach prevention across daemon instance changes;
- detector rehydration from a terminal snapshot and output gap;
- no created/done/failed event caused solely by daemon reconnect;
- explicit recovery runtime-generation change;
- worktree compensation in every create and remove crash phase.

### 26.2 Process integration tests

A real `pohunek-sessiond` and PTY child test program prove:

1. child emits a counter and accepts input;
2. daemon records child PID, PTY identity, worker ID, and runtime ID;
3. daemon receives `SIGTERM`;
4. counter continues while daemon is absent;
5. replacement daemon reconnects;
6. PID and runtime identities are unchanged;
7. output emitted during the outage is replayed or represented by an explicit
   gap/snapshot;
8. input, resize, detector, procwatch, and stop still work.

The same test runs with daemon `SIGKILL`. Additional tests kill one worker and
assert other sessions continue, kill the child while the daemon is absent,
exercise every create crash injection point, and verify orphan/conflict
quarantine.

Tests inspect journals, logs, and process environments only for allowlisted
field names and assert that seeded secret values, prompts, and terminal payloads
are absent.

### 26.3 systemd integration tests

On a disposable user manager:

- install substituted units and verify them;
- start at least two session workers through D-Bus;
- run `systemctl --user restart pohunekd.service`;
- send `SIGKILL` to `pohunekd`;
- upgrade daemon and worker files in place;
- roll daemon forward and back one worker-aware release;
- assert worker unit `MainPID`, child PID, cgroup, PTY identity, and output
  continuity remain unchanged;
- assert stopping the daemon unit does not enqueue worker stop jobs;
- assert stopping one worker unit kills only its control group;
- assert `Restart=no` leaves a killed worker inactive;
- assert daemon `READY=1` follows reconciliation and worker bootstrap readiness
  has no D-Bus deadlock.

### 26.4 Migration and release tests

Fixtures cover:

- legacy resumable agent;
- legacy live session without native identity;
- legacy shell;
- legacy worktree binding;
- interrupted migration manifest;
- explicit runtime-loss consent;
- unsupported boundary rollback;
- worker-aware N/N-1 logical store and journal;
- glibc and MUSL archives containing both binaries and all units;
- installer upgrade preserving live unit PIDs;
- uninstaller refusal with live workers.

### 26.5 Repository gates

The completed implementation passes:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo build --workspace --release
cargo xtask ts check
cargo xtask docs check
```

It also passes the web typecheck, lint, unit tests, browser end-to-end tests, and
real-daemon web suite from `AGENTS.md`. Dependency and feature changes run the
repository's audit and feature-powerset jobs.

## 27. Definition of Done

The architecture is complete only when all of these statements are demonstrably
true:

- a real Codex session, Claude session, and shell session each retain the same
  PTY and child PID across graceful daemon restart, daemon `SIGKILL`, and daemon
  binary upgrade;
- output produced while the daemon is absent is recovered without a silent
  byte gap;
- replacement daemon attach, input, delayed submit, resize, activity detection,
  process observation, and explicit stop work against the existing runtime;
- no daemon shutdown path signals, stops, or restarts a worker;
- one worker failure does not affect any other session;
- create, stop, remove, and child-exit crash windows reconcile deterministically;
- logical records never disappear solely because the daemon restarted;
- launch native identity cannot be overwritten by a nested same-provider agent;
- self-attach is rejected after daemon replacement using stable worker origin;
- native recovery is explicit and visibly creates a new runtime generation;
- supported N/N-1 upgrade and rollback preserve live worker and child PIDs;
- the first legacy migration refuses silent live-runtime loss;
- no secret-bearing value or raw terminal content appears in worker journals or
  structured logs;
- release archives and installation contain and validate both daemon and worker
  runtime assets;
- the full test matrix and repository gates pass;
- architecture, public API, operational, release, and assistant knowledge
  documentation describe the implemented behavior.

## 28. Rejected Alternatives

### 28.1 Change only systemd `KillMode`

`KillMode=process` can leave descendants alive when the daemon stops, but the
daemon still owns the PTY master, reader, child handle, and output history.
Dropping the master closes the runtime control path and commonly sends terminal
hangup to the child. A surviving orphan process is not an attachable session.

### 28.2 Automatic provider-native resume

Native resume starts a new process and PTY, changes PID and runtime state, and
requires a captured provider reference. It cannot preserve shells, uncaptured
sessions, in-flight TUI state, or exact terminal contents. It remains an
explicit disaster-recovery mechanism, not normal daemon restart behavior.

### 28.3 One shared PTY broker

A single persistent broker would move the failure boundary but retain one
process whose crash loses every session. Per-session workers provide the
required isolation and map cleanly to systemd cgroups and resource accounting.

### 28.4 File-descriptor handoff

Passing PTY descriptors with `SCM_RIGHTS` can support a coordinated graceful
handoff, but not `SIGKILL`, panic, OOM kill, or an upgrade after the old daemon
has already disappeared. It also does not transfer portable-pty child reaping,
reader state, terminal parser state, and input dedup safely.

### 28.5 Hot `exec` of the daemon

An exec-based upgrade is limited to intentional graceful replacement and
requires preserving all relevant descriptors and rebuilding asynchronous and
blocking runtime state in place. It provides no crash durability and tightly
couples upgrade mechanics to every daemon resource.

### 28.6 tmux or screen

An external terminal multiplexer could own PTYs, but would make pohunek depend
on another user-facing state model, command parser, socket lifecycle, terminal
history policy, and installation. It would not supply the typed runtime,
identity, transaction, and protocol guarantees required here.

### 28.7 Worker auto-restart

Once a worker dies, its PTY master and in-memory terminal state are gone.
Restarting `pohunek-sessiond` can only create a different PTY. Automatic restart
would mislabel recovery as continuity and risks launching duplicate agent
processes. The correct state is `runtime_lost`, followed by optional explicit
native recovery.
