# pohunek Architecture

This document describes the application architecture for `pohunek`.

## Status and Scope of This Revision

`idea.md` captures the original, broad product brainstorm. This document is the
**authoritative current direction** and intentionally narrows that vision into a
single, coherent, buildable tool. Where this document and `idea.md` disagree,
this document wins.

The committed direction is a **single-user, personal multi-host tool**: one
operator (you) running durable coding-agent sessions across your own machines,
which are connected by a **NetBird** (WireGuard) private network.

Key consequences of that scope, decided explicitly:

- **No multi-user authorization.** The machines and the network are yours. SSH
  bridging, signed mesh manifests, key rotation, and tamper-evident audit logs
  from the original plan are **out of scope**. The network (NetBird/WireGuard)
  and ordinary filesystem permissions are the trust boundary.
- **Remote transport is direct over NetBird**, not an SSH bridge.
- **Agents run PTY/TUI-first** (real terminals). `pohunek` is a
  terminal multiplexer for agents, not a re-rendered control plane.
- **Discovery is tokenless NetBird-local**, with live capability queries instead
  of signed manifest exchange.
- **The GUI is deferred.** Interactive control happens by attaching to a session
  from your existing terminal. The next GUI path is a Rust SDK followed by a
  pure-native Rust desktop companion app. The browser control center is later and
  optional; the daemon gains no GUI surface.
- **Provider integration (Linear/GitHub) is deferred and shell-out based**
  (`gh`, Linear GraphQL/MCP), not maintained in-tree adapters, and lives in the
  client surfaces (the Phase 5 sway scripts and the Phase 4 browser backend),
  never in the chassis.

## Goals

- Provide a CLI-first control plane for durable coding-agent work across your own
  machines on a NetBird network.
- Keep every meaningful workflow available through `pohunek` commands with
  human-readable defaults and machine-readable `--json` output.
- Run without a central application server. The CLI talks directly to a daemon on
  each host (locally over a Unix socket, remotely over NetBird).
- Make each host authoritative for its own PTYs, agent processes, state, logs,
  and worktrees.
- Support durable detach and reattach by giving every live session a dedicated
  worker whose PTY and child lifecycle is independent of the restartable daemon.
- Use real PTY/TUI agent sessions for Codex, Claude Code, and the local
  interactive Hermes Agent backend, with agent
  state derived from OSC terminal titles first, screen-content pattern matching
  as fallback, and PTY activity for the working signal. Hooks capture only the
  native session ID for resume (Codex/Claude Code do not report live state via
  hooks).
- Be agent-operable: an operator agent can drive the whole tool through the same
  `--json` CLI and subscription API a human uses.

## Non-Goals

- Multi-user authorization or a shared-host trust model. Single operator only.
- A central coordinator, SaaS control plane, or hosted dashboard. Future desktop
  and browser clients are **user-run clients** that hold no authoritative state;
  each host's daemon stays authoritative, and the CLI keeps working directly.
- SSH bridging as the remote transport (NetBird direct transport replaces it).
- A cryptographic mesh: signed manifests, snapshot reconciliation, key rotation.
- In-tree provider adapters in the core path (shell-out instead).
- A GUI in the first version. GUI work is deferred to the SDK-first path in
  `docs/ROADMAP.md`: native desktop first, browser later/optional.
- ACP as the first agent runtime (deferred; PTY/TUI-first).
- WebSocket as the core daemon protocol.

## High-Level Architecture

```text
  CLI (local)                         CLI (remote)
       |                                   |
       | Unix socket                       | TCP over NetBird/WireGuard
       | ($XDG_RUNTIME_DIR, mode 0600)     | (daemon binds ONLY to 100.x iface)
       v                                   v
 +-----------------------------------------------------------+
 |               host control plane (pohunekd)               |
 | public protocol | logical state | events | reconciliation  |
 +-----------------------------------------------------------+
       |
       | private owner-only Unix protocol
       v
 +-----------------------------------------------------------+
 | pohunek-session@s-01J00000000000000000000000.service (one worker per live session) |
 | PTY master | child handle | output ring | terminal tracker |
 +-----------------------------------------------------------+
       |
   Codex / Claude Code / Hermes Agent running in worker-owned PTYs
       |
   identity hooks --> worker; notifications --> daemon
```

The host-local pohunek service is authoritative for live work. `pohunekd` is the
logical-session authority and public control plane; a `pohunek-sessiond` worker
is authoritative for one live PTY generation. There is no shared mesh state and
no central coordinator. A remote host is reached by connecting the CLI directly
to that host's daemon over NetBird using the same public protocol as the local
Unix socket. Workers are never remotely addressable.

## Host Daemon

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
  - a **TCP listener bound to the NetBird interface** for remote clients.

The daemon must not depend on libghostty and never owns or receives a PTY file
descriptor. It proxies attach, input, resize, bounded screen/output observation,
inspect, and stop operations to the worker that owns the runtime. It also owns
the race-free `session.wait` resource model: snapshot, register, recheck, then
sleep without holding the registry write lock.

Daemon startup loads configuration and session metadata before accepting
commands. Missing or invalid required configuration fails startup with a clear
error rather than falling back to unsafe defaults.

### Concurrency and supervision

- The daemon uses Tokio. Each runtime is isolated in
  `pohunek-session@<session-id>.service`, so one worker failure cannot terminate
  the daemon or any other session.
- `pohunekd.service` and worker units are siblings. Workers are grouped under
  `pohunek-sessions.slice`, use `Restart=no`, and have no `PartOf`, `BindsTo`,
  or other stop-propagation dependency on the daemon.
- A stale public Unix socket is detected and replaced on daemon startup, and a
  single-instance lock prevents two daemons controlling the same state
  directory. The daemon sends systemd `READY=1` only after store load, worker
  discovery, reconciliation, and public socket bind.

## Durable Session Workers

Every live logical session has one opaque `worker_id` and one `runtime_id`.
The worker owns the PTY master, root child handle, reader and reaper, bounded raw
output ring, terminal tracker, input deduplication, resize sequencing, and final
outcome. The daemon owns the stable session id, launch snapshot, project and
worktree association, native recovery reference, desired state, and lifecycle
transaction.

Disconnecting or killing `pohunekd` releases only the private controller lease.
The worker continues draining output and running the unchanged child. A
replacement daemon enumerates worker units, journals, and sockets, validates
the systemd `MainPID` and process start identity, negotiates the private
protocol, acquires the one-controller lease, calls `Inspect`, and reconstructs
detector and procwatch state. It does not invoke native resume.

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

There is one logical protocol exposed over two transports:

- **Local:** Unix domain socket at `$XDG_RUNTIME_DIR/pohunek/daemon.sock`
  (directory mode `0700`, socket mode `0600`). This is the only access control
  needed for the single-user model: the socket is owner-private.
- **Remote:** one TCP listener per configured overlay, bound **only** to the
  provider's validated current local member address and per-overlay port, never
  `0.0.0.0`. Reachability and authentication are provided by that overlay;
  NetBird/WireGuard is the default production provider.

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
onto both the Unix socket and the NetBird TCP transport. Multiple clients may
attach to one session; resize policy when sizes differ is defined by the daemon
(last attach wins, with explicit resize control available).

If the private worker stream reports a typed failure, the daemon closes the raw
pipe and retains that sanitized failure in a short-lived bounded result mailbox.
The client consumes it once through `session.detach` and stops reconnecting that
deterministic failure. Raw PTY bytes therefore remain unframed without losing
the worker's root cause.

## PTY/TUI Agent Runtime

The runtime path is real PTY/TUI agent sessions. **Codex, Claude Code, and the
local interactive Hermes Agent backend are first-class**. Hermes support is
pinned to version 0.20.0 and deliberately excludes its remote, browser,
desktop, gateway, ACP, and other non-local backends: Pohunek must own the PTY,
process, worktree, and diff on the same host.

Runtime responsibilities:

- Start one agent subprocess inside each worker-owned PTY.
- Preserve actual terminal interaction for attached clients.
- Allow clients and the daemon to disconnect without killing agents.
- Track `idle`, `working`, `blocked`, `done`, and `failed` states.
- Store a validated immutable native recovery reference without using it for
  normal daemon reconnection.
- Bind each session to host, project, repository, worktree, branch, agent type,
  logs, events, and resume metadata.

### Agent state detection

This was validated against the source of `herdr` (same Rust + portable-pty +
Tokio stack) and `Kandev`. Correction to the early assumption: **Codex and Claude
Code do not report live state via hooks** — herdr explicitly removes their
lifecycle hooks as unreliable, and Kandev derives PTY state from a virtual
terminal emulator. State is derived from the terminal stream, in priority order:

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

States tracked: at least `working`, `blocked`/`waiting_approval`, `idle`, `done`,
`failed`, each carrying a `source` field for signal strength. Detection rules are
**data (TOML manifests)** so agents can be added or fixed without recompiling. The
`blocked` signal is the trickiest and is validated per agent and agent-CLI
version.

Hooks have two separate roles:

- **Native recovery binding.** A launch-agent `SessionStart` hook prefers the
  worker socket and posts the agent's session ID or transcript path. The worker
  validates process ancestry and accepts this binding only for the designated
  immutable launch agent; it journals the accepted value before forwarding it
  to the daemon. This keeps recovery tied to the original launch identity and
  retains the claim across daemon outage. If the private endpoint cannot be
  reached, the hook uses the local public daemon as a hardened fallback. That
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

Live state remains detector-first: OSC, screen, PTY activity, and process state
continue to drive normal activity transitions. Notification hooks still target
the daemon and may be missed during daemon outage. A nested active-agent report is
explicit hook evidence and uses the `report` state source when it supplies
activity. While such a report is current, the detector can temporarily switch to
the active agent's manifest so Codex/Claude UI patterns are interpreted
correctly inside the shell session. Releasing the active report clears the
active fields and restores the shell/default detector manifest. See the resume
model below.

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
- **Hermes Agent 0.20.0:** the adapter uses bracketed paste and sends submit as
  a separate `\r` write after 150 ms. The release compatibility gate must
  contain reviewed real-PTY evidence for the classic interface and either a
  capture or a recognized local-unavailable diagnosis for the alternate-screen
  TUI before this support is released. Programmatic input accepts LF and tab
  but rejects other terminal controls; it is refused while a visible
  owner-approval prompt is blocked.
- **Other agents:** use the agent adapter's framing rules.

These per-agent input rules live in the agent adapter next to its launch command,
state manifest, and resume command.

### Hermes operator plugin

The pinned local Hermes Agent `0.20.0` runtime may load a Pohunek-owned plugin
from one explicitly selected Hermes profile. This is a CLI/client-side
integration; the daemon stays provider-neutral and M3 does not increase the
public protocol version. The release `pohunek` binary embeds the plugin assets
and the deterministically generated `pohunek:pohunek` skill. Installation uses
only Hermes's supported plugin commands and places a separate owner-private
Pohunek policy outside the immutable plugin checksum set. The rendered plugin
asset stores the exact absolute policy path selected by the installer.

The policy fixes the verified absolute CLI executable, public protocol range,
access mode, host allowlist, and bounded tool limits. It is a delegated-tool
guardrail, not a same-user sandbox: a process with shell or file-write authority
can still bypass or alter same-user files. The plugin accepts no raw argv,
arbitrary daemon method, arbitrary endpoint, raw attach stream, force flag, or
environment map. It invokes a single fixed-argv CLI JSON runner and passes
untrusted prompt/input text only on standard input. Remote requests remain
direct connections to the selected host daemon over NetBird; no plugin listener,
proxy, SSH bridge, or central service is introduced.

Read tools expose hosts, sessions, inspect, rendered screen, bounded output,
wait, and diff. `manage` additionally exposes structured project/worktree/agent
profile session operations with exact unique-name resolution. `full` alone
registers stop and remove. The plugin repeats the daemon's origin-session guard
before subprocess start: it denies exactly `session.stop`, `session.resume`,
`session.remove`, `session.fork`, `session.resize`, `session.set_metadata`,
`session.rename`, and `session.input` when they target the hosting session. The
only origin-session exceptions are the lifecycle reports
`session.report_agent`, `session.release_agent`, and
`session.report_native_id`; the daemon remains authoritative.

Hooks run only in a Pohunek-managed Hermes process. They use a short bounded
local Unix-socket attempt, prefer worker-private identity reporting, and fall
back to the hardened local public native-ID method. They never start a
subprocess, access the network or Hermes `state.db`, emit terminal output, or
copy prompt/tool/output payloads. A failed hook is counted and swallowed, so
process and screen detection remain the daemon's fallback. `on_session_end` is
not a process-exit signal. A higher-sequence continuation identity reported by
`pre_llm_call` supersedes the launch identity for a later native resume.

## Overlay Registry and Discovery

The daemon, SDK, CLI, GUI core, and web backend consume one configured overlay
registry. Each entry has a stable overlay ID, a transport implementation, and
its own non-zero daemon port. Daemon listeners run concurrently for every
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
Provider discovery returns remote peers only; GUI, web, and CLI fan-out
consumers add the explicit local Unix-socket target themselves.

The GUI retains the overlay-qualified peer identity and discovered port, never
the discovered IP as reconnect state. Every control reconnect resolves that
stable identity through current provider state. External attach receives the
same selector with its explicit discovered port; the resulting SDK client keeps
the exact resolved endpoint only long enough to keep control and raw attach on
one route.

CLI fan-out and dynamic completion retain the same canonical identity plus
discovered port, never the cached probe IP. The web relay forces a fresh local
daemon discovery before each remote tunnel upgrade and accepts the cached route
only if the requested identity still owns it. Active daemon overlay listeners
revalidate their provider-owned local address periodically; an address change
binds the replacement before the stale listener is dropped.

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
**its own** per-host store — so no filesystem path ever crosses the wire to a
remote host. See [`docs/design/projects.md`](design/projects.md) for the full
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
in the project's checkout as-is — "open a terminal here, work here." With
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
   record remains visible with `runtime.state=lost`; recovery is explicit and
   creates a new worker, runtime ID, PTY, and child PID.

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
```

Runtime state under the user data directory:

```text
~/.local/share/pohunek/
  metadata.jsonl           # logical sessions, worktrees, and projects (0600)
  notifications/
    notifications.jsonl    # durable notification action log (0600)
    policy.json             # notification, retention, and compaction policy (0600)
  events/                  # local append-only event log (audit/debug, not replicated)
  worktrees/               # managed git worktrees
```

Ephemeral owner-private sockets under the user runtime directory:

```text
$XDG_RUNTIME_DIR/pohunek/
  daemon.sock
  daemon.lock
  workers/<session-id>/control.sock
```

Structured logs under the user state directory:

```text
~/.local/state/pohunek/logs/
  pohunekd.jsonl[.1-.7]                 # 32 MiB each, 256 MiB daemon cap
  pohunek-session-<session-id>.jsonl    # active worker log
  pohunek-session-<session-id>.jsonl[.1-.3]
                                         # 4 MiB each, 16 MiB per-session cap
~/.local/state/pohunek/workers/<session-id>/<worker-id>.json
```

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
crash-atomic. The event log is the local audit/debug trail. None of these is
replicated across hosts — each host's daemon is authoritative and is answered
live (see "High-Level Architecture"). An embedded SQLite `state.db`
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

The worker journal contains runtime identity, process identity, dimensions,
phase, terminal outcome, sanitized identity claims, and output offsets. It does
not contain environment values, prompts, input bytes, terminal bytes, rendered
screens, tokens, or notification bodies. Live output history and terminal
screens remain bounded in worker memory.

## Security Model (Single-User Scope)

The threat model is a single operator on their own machines and NetBird network.
Multi-user authorization is explicitly out of scope. The controls that remain are
cheap, free-by-default, or inherited:

- **Local socket** in a `0700` directory with mode `0600`: owner-private, no
  authorization system needed.
- **Remote listener bound only to the NetBird interface**, never `0.0.0.0`.
  NetBird/WireGuard provides encryption and network authentication; NetBird
  policies decide which peers can reach the daemon port.
- **Origin-session guard.** Requests from managed children carry paired
  `origin_session_id` and `origin_daemon_id` envelope markers. When both identify
  the target as the caller's origin, the daemon rejects exactly `session.stop`,
  `session.resume`, `session.remove`, `session.fork`, `session.resize`,
  `session.set_metadata`, `session.rename`, and `session.input` with
  `plugin_self_target_denied`. Read-only observation is allowed. The lifecycle
  reports `session.report_agent`, `session.release_agent`, and
  `session.report_native_id` are also deliberately allowed: managed hooks must
  report their own session, and the public native-id method is the necessary
  local fallback when the owner-private worker claim cannot be delivered. The
  pair is atomic and is copied to dedicated wait connections. This narrowly
  prevents an in-session automation client from invoking those eight mutations
  against the PTY that hosts it, but it is not authentication: any same-user
  process able to reach the owner socket remains inside the trusted
  single-operator boundary.
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
- NetBird discovery runs and candidate/capability results. The CLI can discover
  locally without `pohunekd`; daemon discovery remains available for GUI/web RPC consumers.
- Agent state transitions with their `source`.
- Latency for CLI commands, attach, discovery, and remote connections.

`pohunek doctor` reports environment, daemon, NetBird, agent CLI, and storage
health for humans and operator agents.

## Error Handling

- Fail closed for resume decisions that would auto-run custom commands.
- Keep unrelated sessions running when one session fails.
- Preserve which host failed in remote error messages.
- Provide suggested recovery commands when known.
- Return structured error data for `--json` commands.
- Distinguish error classes: configuration, daemon (unavailable, version
  mismatch, framing), transport (NetBird unreachable, connection lost), runtime
  (agent binary missing, PTY allocation, process exit, worktree conflict), and
  discovery (NetBird CLI missing, local state unavailable).

## Testing Strategy

Core tests:

- Control protocol serialization for request/response/error/event envelopes and
  version negotiation.
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
  round-trip coverage across create, stop, remove, recovery, and restart.
- CLI table and `--json` output.

Integration tests:

- Local daemon + CLI full session lifecycle.
- Remote daemon over NetBird (or a loopback TCP stand-in) reusing the same
  protocol; attach/detach without killing the remote process.
- Tokenless NetBird discovery from `netbird status --json` fixtures, with
  graceful degradation when the CLI/state is absent.
- Structured logs and event-log records for important transitions.

## Architecture Risks

- **PTY/TUI state detection.** Mitigation: prefer agent hooks over scraping; mark
  state `source`; validate the `blocked` signal empirically.
- **Worker loss destroys one PTY generation.** Mitigation: one isolated
  non-restarting worker per session, durable logical records, explicit lost
  state, and optional operator-triggered native recovery.
- **Ambiguous runtime identity.** Mitigation: validate systemd PID plus process
  start identity and journal fields; quarantine conflicts and never
  automatically kill an ambiguous live worker.
- **NetBird local state format drift.** Mitigation: defensive parsing + recorded
  fixtures; shell out to the documented `status --json` rather than internals.
- **Daemon as a network server on NetBird.** Mitigation: bind only to the NetBird
  interface; rely on NetBird policies; no `0.0.0.0`.
- **Worktree cleanup conflicts.** Mitigation: explicit session ownership and
  recorded bindings; ownership checks before reuse/cleanup.
- **Embedded terminal maturity.** Mitigation: GUI deferred; choose and verify the
  desktop terminal component when Track D starts. The CLI attach path needs no GUI.
- **Agent CLIs change under us.** Mitigation: keep the agent boundary thin; rely
  on documented modes (hooks, native resume) and pin behavior with fixtures.

## What Changed vs. the Original Plan (`idea.md`)

| Area | Original plan | This revision |
|------|---------------|---------------|
| Scope | Multi-host, team-capable | Single-user personal tool |
| Remote transport | SSH bridge | Direct over NetBird/WireGuard |
| Discovery | Tailscale + NetBird + signed manifests | NetBird-local + live capability query |
| Mesh trust | Signed manifests, key rotation, snapshot sync | Dropped (NetBird + fs perms) |
| Audit | Tamper-evident considered | Plain local event log (debug) |
| Agent state | Terminal heuristics | OSC title + screen-manifest + PTY activity (per herdr); hooks only capture the session ID for resume |
| Providers | In-tree Linear/GitHub adapters | Deferred, shell-out (`gh`, Linear GraphQL/MCP) in the client surfaces, not the chassis |
| GUI | libghostty client (MVP5) + spike (MVP0) | Deferred; SDK first, native Rust desktop primary, browser control center later/optional. libghostty client dropped |
| Attach framing | "separate stream mode" (unspecified) | Separate connection per PTY (specified) |
| Agents | Codex + Claude Code | Codex + Claude Code + local-terminal Hermes Agent 0.20.0 |
