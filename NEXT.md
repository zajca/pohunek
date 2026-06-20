# NEXT STEP — Milestone 11: Remote hosts over NetBird (Phase 2, delivered whole)

This file describes, in detail, the immediate next step. It is a handoff for
whoever picks up the work (you, a subagent, or a fresh session).

## Why this is one milestone, not three

Remote transport has **no user value until it is complete end to end**: discovery
without a remote daemon is a list you cannot act on; a TCP listener with no client
routing is an open port nobody dials; routing with no discovery has nothing to
target. So Milestone 11 ships the whole loop — **discover a peer, dial its daemon
over NetBird, run the full session lifecycle, attach/detach** — as a single
milestone. Internally it is built in three slices (A discovery, B listener, C
routing), but "done" is the complete remote workflow, not any one slice.

This milestone delivers Phase 2 as scoped in
[`docs/phases/02-remote-netbird.md`](docs/phases/02-remote-netbird.md). It adds a
second transport for the **unchanged** Phase 1 control + attach protocol — no new
session semantics, no central server, no SSH bridge.

## Goal of milestone 11

From your laptop, drive agent sessions on another of your machines on the NetBird
(WireGuard) mesh, using the same CLI as local sessions:

```bash
zagentmesh host discover                       # peers from `netbird status --json`
zagentmesh host list                           # known/reachable hosts + daemon status
zagentmesh host inspect <host>                 # live capability query over NetBird
zagentmesh session new --host <host> --agent claude
zagentmesh session list --host <host>
zagentmesh attach <host>/<session-id>          # Ctrl-] detaches; remote process survives
zagentmesh status --host <host>
zagentmesh session stop <host>/<session-id>
```

Each host stays authoritative for its own daemon, PTYs, metadata, logs, and live
sessions. The local CLI controls a remote host by dialing that host's daemon over
NetBird. NetBird/WireGuard provides encryption + network auth; NetBird policies
decide which peers may reach the port.

---

## Definition of done (testable)

Grouped by slice, but the milestone is **not done until all of #1–#10 hold
together** and the end-to-end criterion (#9) passes.

### Slice A — NetBird discovery (CLI-side, fixture-tested)

1. **`netbird status --json` is parsed defensively.** A new `crates/netbird`
   adapter runs `netbird status --json` and parses it into typed `Self` +
   `Peer` records. Unknown fields are ignored; optional fields default; a
   malformed line never panics. The format is treated as an **unstable external
   input** (per `docs/architecture.md` "NetBird Discovery") and pinned with
   recorded fixtures.
   *Check:* unit tests over fixtures cover: missing optional fields, unknown
   fields, an offline/idle peer, a peer with no `netbirdIp`, and the
   NetBird-CLI-absent case (binary not on PATH → typed `discovery` error, not a
   panic). Field names below are pinned by fixture, not assumed — see
   "NetBird `status --json` reference".
2. **`host discover` / `host list` work tokenlessly.** `host discover` enumerates
   peers from local NetBird state (no management-API token), probes each
   candidate's daemon port, and classifies every peer as **candidate /
   reachable-daemon / version-mismatch / unreachable**. `host list` shows the
   known hosts with their NetBird address, name, daemon status, and (when known)
   daemon version. Both support `--json` with stable fields for host identity,
   NetBird address, daemon status, and classification.
   *Check:* with a loopback TCP stand-in daemon (CI), `host discover` reports it
   as reachable-daemon; with the port closed, unreachable; with an incompatible
   protocol version, version-mismatch — each a distinct, stable `--json` shape.

### Slice B — Daemon TCP listener bound to NetBird only

3. **The daemon binds a TCP listener to its own NetBird address only.** At
   startup the daemon resolves its NetBird IP (from `netbird status --json`
   `localPeerState.IP`, CIDR stripped) and binds the control listener to
   `<netbird-ip>:<port>`. The bind address is **validated to be inside
   `100.64.0.0/10`** and is **never `0.0.0.0`, a loopback, or a non-NetBird
   interface** — an invalid/absent NetBird address means the daemon runs
   **local-only** (Unix socket still served) and logs why, rather than failing or
   binding wide. Validation **fails closed**.
   *Check:* `validate_netbird_bind_addr` is a pure, unit-tested function:
   accepts `100.64.x.y`–`100.127.x.y`, rejects `0.0.0.0`, `127.0.0.1`, RFC1918,
   and public IPs. An integration test asserts the daemon refuses to bind a
   non-NetBird address and stays local-only when NetBird is absent.
4. **The TCP transport serves the unchanged Phase 1 protocol, including attach.**
   The same newline-JSON control dispatch and the same separate-connection raw
   attach stream work over TCP exactly as over the Unix socket. Version
   negotiation already runs at dispatch (`handler.rs` `negotiate`) and applies
   unchanged across hosts.
   *Check:* an integration test runs `session new/list/inspect/input/stop` and a
   full `attach`→write→`detach` cycle against the daemon over a loopback TCP
   connection; remote attach/detach does **not** kill the remote daemon-owned
   process (assert the session is still listed after detach).

### Slice C — CLI remote routing

5. **`--host` and `host/session-id` actually execute remotely.** The grammar
   already parses (`crates/cli/src/target.rs`, the global `--host` in
   `crates/cli/src/main.rs`); the current `ensure_local_host` rejection
   (`CliError::RemoteNotSupported`) is **replaced** by real remote dispatch. A
   host name resolves to a NetBird address via local `netbird status --json`
   (match on NetBird name/fqdn/alias). Every read/automation command that works
   locally works against `--host <host>` / `<host>/<session-id>`.
   *Check:* the same command produces equivalent `--json` output whether run
   locally or against a loopback-TCP "remote", differing only in host-identity
   fields.
6. **`host inspect <host>` is a live capability query.** A new additive control
   method returns the remote daemon's version, supported agents, available agent
   runtimes (which agent binaries are present on that host), and worktree/repo
   capability hints — obtained live over the NetBird connection, never from a
   cached manifest.
   *Check:* `host inspect` over loopback TCP returns the daemon version + agent
   list; deserializes into the new typed capability struct; `--json` carries the
   stable fields.

### Cross-cutting

7. **Errors name the host and distinguish the failure layer.** Remote failures
   carry the host and a typed class that separates **NetBird reachability**
   (`transport`) from **remote daemon** (`daemon`: unavailable / version-mismatch)
   from **remote agent** (`runtime`), and **NetBird discovery** problems
   (`discovery`: CLI missing / local state unavailable). `request_id` correlates a
   command across both hosts' logs.
   *Check:* forced NetBird-unreachable, remote-daemon-absent, and version-mismatch
   each produce a distinct, stable `code` under `--json`; the human message names
   the host; both hosts' logs show the same `request_id`.
8. **Remote session creation is gated by explicit confirmation.** Per the
   security model (`docs/architecture.md` "Dangerous operations … stay behind
   explicit, per-action confirmation"), `session new --host <remote>` prompts for
   confirmation before starting work on another machine, with a `--yes` (and
   `--json` non-interactive) bypass. Local `session new` is unchanged.
   *Check:* `session new --host <remote>` without `--yes` aborts on a declined
   prompt; with `--yes` it proceeds; under `--json` it requires `--yes` (no
   silent prompt on a machine-readable path).
9. **End-to-end: the whole remote loop works.** With a daemon reachable over
   NetBird (or a loopback TCP stand-in in CI): discover it, start a Codex/Claude
   PTY session on it, attach, detach, reattach, and stop — all from the local CLI,
   over a direct connection, with no central server and no SSH bridge, and with
   the daemon port reachable only on the NetBird interface.
10. `cargo build`, `cargo clippy --all-targets --workspace -- -D warnings`, and
    `cargo test --workspace` stay clean.

### Explicitly OUT of scope (do NOT build here)

- **SSH/Tailscale transports** — NetBird direct only (the SSH bridge is replaced,
  not added).
- **Signed manifests, mesh snapshot sync, key rotation, multi-user auth** —
  dropped from the design; NetBird policy + single-operator scope is the boundary.
- **Provider integrations (Linear/GitHub), libghostty GUI, ACP** → Phase 3
  (`docs/phases/03-later-providers-and-gui.md`).
- **SQLite `state.db` + migrations** → still deferred backlog (the `doctor`
  `schema_version` check stays a `warn`).
- **New session lifecycle features** (worktree cleanup-on-stop, merge flows). No
  change to Phase 1 session semantics — Phase 2 is purely a second transport.

---

## Where we are now (done, verified)

**Phase 1 is complete (milestones 1–10).** `cargo build`, `cargo clippy
--all-targets --workspace -- -D warnings`, and `cargo test --workspace` =
**322 passed**.

- `crates/protocol` — typed control envelopes; full session lifecycle + attach
  types; version negotiation (`negotiate` / `PROTOCOL_VERSION`); typed
  `ProtocolError { class, code, msg, recover }` with `ErrorClass`
  **already including `Transport` and `Discovery`** (the two classes Phase 2
  needs) and canonical constructors (`version_mismatch`, `agent_binary_missing`,
  `bad_request`, `method_not_found`).
- `crates/daemon` (`zagentmeshd`) — Unix-socket control server; full `session.*`
  lifecycle; attach bridge; `subscribe` event stream; per-session state engine;
  agent adapters (codex/claude/shell); session-id hook + resume;
  worktree-per-session; M9 unified metadata store + append-only event log; M10
  typed `agent_binary_missing` on ENOENT spawn.
- `crates/cli` (`zagentmesh`) — `doctor`, `daemon start`, `health`/`status`,
  `session new/list/inspect/stop/input`, `attach`, `integration install`. Global
  `--host` parses (local-only execution). `--json` everywhere, incl. clap usage
  errors (M10 + the `cli_usage` fix). `recover` hints rendered for human + JSON.

### Seams milestone 11 builds on (already in place)

These are the exact attachment points; Phase 2 was designed into Phase 1.

- **CLI grammar is already host-aware.**
  - `crates/cli/src/main.rs` — the global `--host` arg (`Cli`, ~line 35) and
    `ensure_local_host` (~line 325) that currently returns
    `CliError::RemoteNotSupported`. **Slice C replaces that rejection with remote
    dispatch.** Add a `Commands::Host { action: HostAction }` enum here for the new
    `host discover/list/inspect` subcommands.
  - `crates/cli/src/target.rs` — `Target { host, session_id }` with the
    `host/session-id` grammar, `is_local()`, `host_or_local()`. No grammar change
    needed; wire `host` to transport selection.
- **The CLI client is the transport seam.**
  - `crates/cli/src/client.rs` — `LocalClient` wraps a `Framed<UnixStream,
    LinesCodec>` and does one request/response (`connect`, `request`,
    `REQUEST_TIMEOUT`, `MAX_LINE_BYTES`). **Generalize to a transport-agnostic
    client** (enum or trait over `AsyncRead + AsyncWrite + Unpin`) with a
    `RemoteClient` that dials `<netbird-ip>:<port>` via `TcpStream`. The
    request/response and framing logic is identical — only the dial differs.
  - `crates/cli/src/commands/*.rs` — each `run(paths, …)` does
    `LocalClient::connect(&paths.socket)` then `client.request(&req)` (e.g.
    `health.rs:26`). Route through a `Client::connect(target_host, paths)` that
    picks Unix vs TCP from the resolved host. The `Request`/`Response` types are
    unchanged.
- **The daemon dispatch is already transport-generic; the plumbing is not.**
  - `crates/daemon/src/api/handler.rs` — `dispatch_line(&str, &DaemonState)` and
    `handle_request(&Request, &DaemonState)` are **already stream-agnostic**
    (operate on a line + shared state). Version negotiation runs here
    (lines 109, 133). `parse_attach_prelude` (line 315) detects the
    `{"attach": stream_id}` prelude. **Add the `host.inspect`/capabilities method
    to the `match` at line 137** (additive, version-negotiated).
  - `crates/daemon/src/api/mod.rs` — `ControlServer`, `serve_connection`
    (line 161), `run_attach_connection` (197), `run_attach_bridge` (234) are
    **hardcoded to `UnixStream`**. **Refactor them generic over `S: AsyncRead +
    AsyncWrite + Unpin + Send`** (Framed works over any such stream) so a new
    `RemoteServer` (TCP) reuses the exact same connection-serving + attach-bridge
    code. The attach reuse is free: a remote client dials the same NetBird
    host:port a second time and sends the same `{"attach": stream_id}` prelude.
  - `crates/daemon/src/main.rs` — `run()` binds the Unix `ControlServer`
    (line 92) and `serve`s it (line 102). **Add: resolve NetBird IP, validate it,
    bind a `RemoteServer`, and serve both concurrently** (`tokio::join!` / spawn).
    NetBird-absent ⇒ skip the TCP listener, keep local-only.
- **Error taxonomy already fits.** `crates/protocol/src/error.rs` —
  `ErrorClass::{Transport, Discovery}` already exist. Add constructors:
  `netbird_cli_missing`, `netbird_state_unavailable` (class `discovery`),
  `host_unreachable`, `remote_daemon_absent` (class `transport`/`daemon`). The
  CLI's `CliError` grows matching variants; `CliError::RemoteNotSupported` is
  removed.
- `crates/cli/src/commands/doctor.rs` — add a **NetBird check** (CLI present,
  daemon connected, self NetBird IP resolvable) to the existing check list
  (`docs/architecture.md` "doctor reports … NetBird …").

---

## NetBird `status --json` reference (pin against the installed version)

From NetBird CLI docs (`docs.netbird.io/get-started/cli`,
`netbirdio/netbird`) — treat as **unstable**; confirm against a fixture recorded
from the NetBird version actually installed, and tolerate drift:

- Root: `status` (`Connected` | `Disconnected` | `Connecting` | `NeedsLogin` | …),
  `daemonVersion`, `managementState`, `signalState`, `localPeerState`, `peers[]`.
- `localPeerState.IP` — **this host's** NetBird address in **CIDR** form
  (e.g. `100.64.0.10/16`); strip the mask to get the bind IP.
- `peers[]`: `netbirdIp` (the `100.x` address — note the camelCase key),
  `fqdn`, `connectionStatus` (`Connected`/`Connecting`/`Idle`), `connType`
  (`P2P`/`Relayed`), `lastSeen` (ISO-8601), `latency`, `direct` (bool).
- NetBird range: **`100.64.0.0/10`** (RFC 6598 CGNAT); each account gets a `/16`
  within it. Linux interface is `wt0` (configurable, rarely changed).
- NetBird daemon not running / not logged in: non-zero exit and/or `NeedsLogin`
  status — the adapter must classify this as a typed `discovery` error, not crash.

A fixture corpus (`crates/netbird/tests/fixtures/*.json`) is the source of truth
for the parser; bump it deliberately when NetBird changes shape.

---

## Implementation tasks (build order A → B → C, ship together)

1. **`crates/netbird` (new crate).** Subprocess runner for `netbird status
   --json` + defensive Serde types (`NetbirdStatus`, `SelfPeer`, `Peer`) with
   `#[serde(default)]` + unknown-field tolerance; `self_netbird_ip()` and
   `peers()` helpers; typed `discovery` errors for CLI-missing / state-unavailable.
   Fixtures + unit tests. Depended on by both `daemon` (self-IP) and `cli`
   (discovery). Also a pure `validate_netbird_bind_addr(IpAddr) -> Result` here
   (or in protocol) so the daemon and tests share one validator.
2. **Daemon TCP listener (Slice B).** Make `serve_connection` /
   `run_attach_connection` / `run_attach_bridge` in `api/mod.rs` generic over the
   stream type. Add `RemoteServer::bind(SocketAddr, DaemonState)` reusing the
   generic serving code. In `main.rs`, resolve + validate the NetBird IP, bind the
   `RemoteServer`, and serve it alongside the Unix `ControlServer`; degrade to
   local-only when NetBird is absent. Default port is a **named const with an env
   override** (e.g. `ZAGENTMESH_REMOTE_PORT`) — no bare literal (project rule:
   zero hardcoded values).
3. **`host.inspect` capability method (Slice B/C boundary).** Add an additive
   control method + typed result (daemon version, supported agents, present agent
   runtimes, repo/worktree hints) to `protocol` and `handler.rs`.
4. **CLI transport generalization (Slice C).** Generalize `client.rs` into a
   `Client` that connects over Unix (local) or TCP (`<netbird-ip>:port`). Add host
   resolution (name/fqdn → NetBird IP via `crates/netbird`). Route every command's
   `run(...)` through it. Remove `ensure_local_host`/`RemoteNotSupported`.
5. **`host` subcommands + `--host` execution (Slice C).** Add `Commands::Host`
   with `discover` / `list` / `inspect`. Implement classification (candidate /
   reachable-daemon / version-mismatch / unreachable) via TCP probe +
   `daemon.health` + negotiation. Add the remote-session confirmation gate
   (DoD #8).
6. **Errors, doctor, observability.** Wire the typed transport/discovery errors
   end-to-end with host name + `request_id`; add the NetBird `doctor` check; add
   structured logs + event-log records for remote connections, discovery runs, and
   remote failures.

---

## Tests (must pass before done)

- **netbird adapter:** parse fixtures (missing/unknown/optional fields, offline
  peer, no `netbirdIp`, CLI-absent); `self_netbird_ip` strips CIDR; bind-addr
  validator accepts `100.64`–`100.127.x.y` and rejects `0.0.0.0`/loopback/RFC1918/
  public.
- **daemon over loopback TCP (CI stand-in):** full `session.*` lifecycle +
  `attach`→write→`detach` over `TcpStream`; detach leaves the remote process
  alive; the daemon refuses a non-NetBird bind address and stays local-only when
  NetBird is absent; `host.inspect` returns version + agents.
- **CLI routing:** host-target normalization (local vs `host/session-id`); a
  command's `--json` output matches between local and loopback-"remote" except for
  host-identity fields; remote `session new` confirmation gate (declined / `--yes`
  / `--json`-requires-`--yes`).
- **discovery classification:** reachable-daemon / version-mismatch / unreachable
  each a distinct stable `--json` shape; NetBird-CLI-absent is a typed `discovery`
  error.
- **error layering + correlation:** NetBird-unreachable vs remote-daemon-absent vs
  version-mismatch produce distinct stable `code`s naming the host; `request_id`
  appears in both hosts' logs.
- Keep `cargo build`, `cargo clippy --all-targets --workspace -- -D warnings`, and
  `cargo test --workspace` clean.

---

## After this milestone

Phase 2 is complete: tokenless NetBird discovery, live capability inspection, and
the full remote PTY/TUI session lifecycle for Codex and Claude Code over a direct
NetBird connection — no central server, no SSH bridge, port reachable only on the
NetBird interface. The control + attach protocol is unchanged, so **Phase 3**
(deferred, optional) can add provider integrations (Linear/GitHub) and a
libghostty GUI without protocol changes. See
[`docs/phases/03-later-providers-and-gui.md`](docs/phases/03-later-providers-and-gui.md).
