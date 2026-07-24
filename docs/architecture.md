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
- Use real PTY/TUI agent sessions for both Codex and Claude Code, with agent
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
   Codex / Claude Code running in worker-owned PTYs
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
descriptor. It proxies attach, input, resize, inspect, and stop operations to
the worker that owns the runtime.

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

## Transport and Control Protocol

There is one logical protocol exposed over two transports:

- **Local:** Unix domain socket at `$XDG_RUNTIME_DIR/pohunek/daemon.sock`
  (directory mode `0700`, socket mode `0600`). This is the only access control
  needed for the single-user model: the socket is owner-private.
- **Remote:** TCP listener bound **only** to the host's NetBird address
  (`100.x.y.z`), never `0.0.0.0`. Reachability and authentication are provided by
  NetBird/WireGuard; which peers may reach the port is governed by NetBird
  policies.

The control protocol is **newline-delimited JSON**: one JSON request per line,
one JSON response line for ordinary requests. Long-lived subscriptions keep the
connection open and stream event envelopes as newline-delimited JSON. Requests,
responses, errors, and events are typed Rust structs serialized with Serde.

Every control request carries a `request_id`; responses and related events echo
it so cross-host operations are traceable in both hosts' logs.

### Protocol versioning

Control envelopes carry a protocol version. New fields are additive and unknown
fields are ignored, so a newer CLI and an older daemon interoperate for the
common subset. On connect, client and daemon exchange versions; a genuinely
incompatible pair fails with a clear, typed error instead of undefined behavior.

## Attach Streaming

Raw terminal bytes are never multiplexed onto the newline-delimited JSON control
connection. Attach uses a **separate connection**:

1. On the control connection the client sends `attach { session_id }`.
2. The daemon replies with a `stream_id` (and, for TCP, the port/token to dial).
3. The client opens a second connection, sends a small header identifying
   `stream_id`, and the connection becomes a raw, bidirectional byte pipe:
   terminal output flows down, the client's keystrokes flow up.
4. `resize`, `detach`, and other control actions are sent on the **control**
   connection, referencing `session_id` / `stream_id`, so they work while
   attached without escaping the byte stream.

This keeps JSON as JSON and bytes as bytes, is trivial to debug, and maps cleanly
onto both the Unix socket and the NetBird TCP transport. Multiple clients may
attach to one session; resize policy when sizes differ is defined by the daemon
(last attach wins, with explicit resize control available).

## PTY/TUI Agent Runtime

The runtime path is real PTY/TUI agent sessions. **Codex and Claude Code are both
first-class from the start**; supporting both immediately validates the agent
adapter boundary.

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
  retains the claim across daemon outage.
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
- **Other agents:** use the agent adapter's framing rules.

These per-agent input rules live in the agent adapter next to its launch command,
state manifest, and resume command.

## NetBird Discovery

Discovery is tokenless and NetBird-local. There is no signed manifest exchange.

1. The daemon/CLI reads local NetBird state via `netbird status --json` (no
   management-API token) to enumerate peers, NetBird addresses, and names.
2. Candidate peers are probed to see which run a reachable `pohunek` daemon.
3. Capabilities are obtained by a **live query** to the target daemon over the
   direct NetBird connection (`host inspect`), not from a cached manifest.

NetBird's `status --json` format is treated as an unstable external input: parsed
defensively (optional fields default, unknown fields ignored) and pinned with
recorded fixtures in tests. NetBird/VPN names and addresses are display and
routing hints only.

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
~/.local/state/pohunek/workers/<session-id>/<worker-id>.json
```

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

Structured logs under `~/.local/state/pohunek/logs/`, redacting secrets and
sensitive terminal content. Useful signals:

- Daemon startup/shutdown, reconciliation, and single-instance/socket recovery.
- Worker bootstrap, controller lease, runtime identity, output-gap, child-exit,
  terminal acknowledgement, and shutdown outcomes.
- Control request summaries (with `request_id`) and response status.
- Session start, attach, detach, stop, process exit, daemon reconnection, worker
  loss/conflict, and explicit native recovery.
- PTY allocation, resize, stream errors, worker protocol versions, and
  controller reconnect latency.
- NetBird discovery runs and candidate/capability results.
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
| Agents | Codex + Claude Code | Codex + Claude Code (unchanged) |
