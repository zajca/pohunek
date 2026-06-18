# zagentmesh Architecture

This document describes the application architecture for `zagentmesh`.

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
- **Agents run PTY/TUI-first** (real terminals). `zagentmesh` is a
  terminal multiplexer for agents, not a re-rendered control plane.
- **Discovery is tokenless NetBird-local**, with live capability queries instead
  of signed manifest exchange.
- **The GUI is deferred.** Interactive control happens by attaching to a session
  from your existing terminal. A native libghostty GUI remains the eventual
  target but is built only after the core is in daily use.
- **Provider integration (Linear/GitHub) is deferred and shell-out based**
  (`gh`, Linear MCP/API), not maintained in-tree adapters.

## Goals

- Provide a CLI-first control plane for durable coding-agent work across your own
  machines on a NetBird network.
- Keep every meaningful workflow available through `zagentmesh` commands with
  human-readable defaults and machine-readable `--json` output.
- Run without a central application server. The CLI talks directly to a daemon on
  each host (locally over a Unix socket, remotely over NetBird).
- Make each host authoritative for its own PTYs, agent processes, state, logs,
  and worktrees.
- Support durable detach and reattach by letting a background daemon own PTYs and
  process lifecycle.
- Use real PTY/TUI agent sessions for both Codex and Claude Code, with agent
  state derived from OSC terminal titles first, screen-content pattern matching
  as fallback, and PTY activity for the working signal. Hooks capture only the
  native session ID for resume (Codex/Claude Code do not report live state via
  hooks).
- Be agent-operable: an operator agent can drive the whole tool through the same
  `--json` CLI and subscription API a human uses.

## Non-Goals

- Multi-user authorization or a shared-host trust model. Single operator only.
- A central coordinator, SaaS control plane, or hosted dashboard.
- SSH bridging as the remote transport (NetBird direct transport replaces it).
- A cryptographic mesh: signed manifests, snapshot reconciliation, key rotation.
- In-tree provider adapters in the core path (shell-out instead).
- A native GUI in the first version (deferred; libghostty remains the target).
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
 |                     host daemon (Rust)                    |
 |                                                           |
 |  control protocol: newline-delimited JSON (serde)         |
 |  attach stream:    separate raw byte connection per PTY   |
 |                                                           |
 |  +-----------+  +-----------+  +-----------+  +---------+  |
 |  | PTY +     |  | metadata  |  | event log |  | NetBird |  |
 |  | agents    |  | (files)   |  | + logs    |  | discovery|  |
 |  +-----------+  +-----------+  +-----------+  +---------+  |
 +-----------------------------------------------------------+
       |
   Codex / Claude Code running in daemon-owned PTYs
       |
   agent hooks/notifications --> daemon control socket (state)
```

The daemon is the authority for live work on its host. There is no shared mesh
state and no central coordinator. A remote host is reached by connecting the CLI
directly to that host's daemon over the NetBird network using the same protocol
the local CLI uses over the Unix socket.

## Host Daemon

The host daemon is the local control plane for one machine, written in Rust.

Core responsibilities:

- Own OS PTYs, agent processes, and session lifecycle.
- Keep sessions alive when foreground clients disconnect.
- Track session metadata, worktree bindings, agent type, and runtime state.
- Store local metadata in file-based stores (JSON-lines today; an embedded SQLite
  store is a deferred optimization — see "Configuration, State, and Log Storage").
- Write structured logs and append session-lifecycle events to a local event log.
- Record and use native agent session IDs for resume.
- Serve two listeners with one protocol:
  - a **Unix socket** for local clients;
  - a **TCP listener bound to the NetBird interface** for remote clients.

The daemon must not depend on libghostty. It owns PTYs through the OS and streams
terminal bytes to whichever client is attached.

Daemon startup loads configuration and session metadata before accepting
commands. Missing or invalid required configuration fails startup with a clear
error rather than falling back to unsafe defaults.

### Concurrency and supervision

- The daemon uses Tokio. Each session is isolated so a panicking or crashing
  session cannot take down the daemon or other sessions.
- The daemon is expected to run as a **systemd user service** (Linux-first). A
  stale Unix socket from a previous run is detected and replaced on startup, and
  a single-instance lock prevents two daemons owning the same state directory.

## Transport and Control Protocol

There is one logical protocol exposed over two transports:

- **Local:** Unix domain socket at `$XDG_RUNTIME_DIR/zagentmesh/daemon.sock`
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

- Start agent subprocesses inside daemon-owned PTYs.
- Preserve actual terminal interaction for attached clients.
- Allow clients to detach without killing agents.
- Track `idle`, `working`, `blocked`, `done`, and `failed` states.
- Store native agent session IDs and prefer native resume over replaying
  commands.
- Bind each session to host, repository, worktree, branch, agent type, logs,
  events, and resume metadata.

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

**Hooks capture only the native session ID** (for resume), not live state: a
`SessionStart` hook posts the agent's session ID / transcript path to the daemon
socket, fire-and-forget. See the resume model below.

### Agent input injection (TUI quirks)

Sending a prompt into an agent's PTY is not simply "write bytes + `\r`". From
Kandev's hard-won handling:

- **Claude Code (Ink TUI):** disable bracketed paste and send the submit byte
  (`\r`) as a **separate write after a ~150 ms delay**, or Ink's paste-burst
  detection absorbs the Enter into the pasted text and the prompt never submits.
- **Other agents (Codex, etc.):** wrap multi-line prompts in bracketed paste
  (`ESC[200~` … `ESC[201~`) so embedded newlines are not treated as premature
  Enter; submit with `\r`.

These per-agent input rules live in the agent adapter next to its launch command,
state manifest, and resume command.

## NetBird Discovery

Discovery is tokenless and NetBird-local. There is no signed manifest exchange.

1. The daemon/CLI reads local NetBird state via `netbird status --json` (no
   management-API token) to enumerate peers, NetBird addresses, and names.
2. Candidate peers are probed to see which run a reachable `zagentmesh` daemon.
3. Capabilities are obtained by a **live query** to the target daemon over the
   direct NetBird connection (`host inspect`), not from a cached manifest.

NetBird's `status --json` format is treated as an unstable external input: parsed
defensively (optional fields default, unknown fields ignored) and pinned with
recorded fixtures in tests. NetBird/VPN names and addresses are display and
routing hints only.

## Worktree Isolation

Concurrent agent work uses git worktrees. A session binds:

- host;
- repository;
- base branch;
- working branch;
- worktree path;
- assigned agent;
- logs and events;
- resume binding.

Worktree-per-session prevents two agents from sharing one working tree by
accident. Session ownership of a worktree is explicit and recorded in metadata;
the daemon checks ownership before reusing or cleaning up a worktree.

## State and Resume Model

Durability tiers (honest about limits):

1. **Client detach:** PTY and process continue because the daemon owns them.
2. **Client restart:** session list, metadata, and layout restore; reattach to
   live PTYs.
3. **Daemon restart:** live PTYs and arbitrary processes do **not** survive.
   Session metadata, worktrees, and resumable agent conversations remain, and
   sessions can be resumed via native agent session IDs. A daemon upgrade is a
   session-killing event by design; document this in operator workflow.
4. **Host restart:** worktrees, metadata, and resumable agent conversations
   remain; live processes do not.

Resume safety rules:

- Never snapshot environment secrets.
- Prefer native agent resume IDs over replaying shell commands.
- Require explicit approval before auto-running any custom resume command.

## Configuration, State, and Log Storage

Configuration under the user config directory:

```text
~/.config/zagentmesh/
  config.toml
```

Runtime state under the user data directory:

```text
~/.local/share/zagentmesh/
  resume-bindings.jsonl    # sessions + resume metadata (JSON lines, 0600)
  worktree-bindings.jsonl  # worktree bindings (JSON lines, 0600)
  events/                  # local append-only event log (audit/debug, not replicated)
  worktrees/               # managed git worktrees
```

Structured logs under the user state directory:

```text
~/.local/state/zagentmesh/logs/
```

The metadata stores are file-based (JSON lines, atomic temp+rename, `0600`); the
event log is the local audit/debug trail. None of these is replicated across
hosts — each host's daemon is authoritative and is answered live (see "High-Level
Architecture"). An embedded SQLite `state.db` (schema-versioned, with forward
migrations) is a **deferred** option, to be adopted only if scale or query needs
justify it; the schema is sketched in `docs/plan-phase-1.md` ("Deferred: SQLite
Schema").

Secrets are never written to the metadata stores, the event log, or session
metadata. They
stay in the OS keychain, provider CLIs (`gh`, etc.), the SSH/agent environment,
or explicit local environment files that are not committed.

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
  new authorization layer in `zagentmesh`. Dangerous operations (e.g. starting
  sessions on other hosts) stay behind explicit, per-action confirmation.
- **Secrets in terminal output are accepted, not guaranteed-redacted.** Agents
  may print tokens to the PTY; scrollback on your own disk may therefore contain
  secrets. The "no secrets" guarantee applies to *structured* metadata
  (the metadata stores, events, config), not to raw terminal streams. Scrollback is stored
  owner-private; sessions may opt out of scrollback persistence.

External text (terminal output, provider responses) is data, never instructions
to the control plane.

## Observability

Structured logs under `~/.local/state/zagentmesh/logs/`, redacting secrets and
sensitive terminal content. Useful signals:

- Daemon startup/shutdown and single-instance/socket recovery.
- Control request summaries (with `request_id`) and response status.
- Session start, attach, detach, stop, process exit, resume attempts.
- PTY allocation, resize, and stream errors.
- NetBird discovery runs and candidate/capability results.
- Agent state transitions with their `source`.
- Latency for CLI commands, attach, discovery, and remote connections.

`zagentmesh doctor` reports environment, daemon, NetBird, agent CLI, and storage
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
- Separate attach-stream connection: raw bytes survive arbitrary content; control
  actions work while attached.
- Agent state mapping from hook signals (with deterministic fixtures), including
  the `blocked` case; heuristic fallback.
- Resume via native session IDs for Codex and Claude Code where installed.
- Worktree binding, ownership checks, and conflict handling.
- Metadata-store round-trip and restart survival (a single consistent write-path
  for resume + worktree records; SQLite schema/migration deferred with the store).
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
- **Daemon restart kills live processes.** Mitigation: documented durability
  tiers, native resume IDs, approval-gated custom resume.
- **NetBird local state format drift.** Mitigation: defensive parsing + recorded
  fixtures; shell out to the documented `status --json` rather than internals.
- **Daemon as a network server on NetBird.** Mitigation: bind only to the NetBird
  interface; rely on NetBird policies; no `0.0.0.0`.
- **Worktree cleanup conflicts.** Mitigation: explicit session ownership and
  recorded bindings; ownership checks before reuse/cleanup.
- **libghostty (future GUI) maturity.** Mitigation: GUI deferred; re-verify
  libghostty status at build time; the CLI attach path needs no GUI.
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
| Providers | In-tree Linear/GitHub adapters | Deferred, shell-out (`gh`, Linear) |
| GUI | libghostty client (MVP5) + spike (MVP0) | Deferred; libghostty still the target |
| Attach framing | "separate stream mode" (unspecified) | Separate connection per PTY (specified) |
| Agents | Codex + Claude Code | Codex + Claude Code (unchanged) |
