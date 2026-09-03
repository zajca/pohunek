# Track B — Web Control Center plan (2026-07-22)

> **Status and supersession (2026-09-03):** M1 shipped as an owner-only,
> mesh-local transparent Bun backend and remains documented here as current
> behavior. This is not the accepted public team relay. The future trusted Rust
> relay and its multi-team clients follow the
> [team-relay RFC](team-relay-control-plane-rfc.md) and
> [#56](https://github.com/zajca/pohunek/issues/56). The Bun backend remains the
> supported owner-path WebUI; [#86](https://github.com/zajca/pohunek/issues/86)
> adds a separate team relay surface rather than replacing it.
> Standalone and direct NetBird owner paths remain first-class.

Implements **Track B** from [`ROADMAP.md`](../ROADMAP.md), designed in
[`phases/04-browser-control-center.md`](../phases/04-browser-control-center.md).
That phase doc remains the source of truth for *why*; this plan reconciles its
decisions with what has shipped since it was written (Track S complete, Track D
shipped through D.6, `@pohunek/relay` existing as a tested 1:1 WS tunnel) and
splits the track into milestones. Slice A (SDKs) is done; this plan covers
Slices B–E.

Prerequisites already complete:

- **S.1–S.3** — Rust SDK, versioned public API ([`public-api.md`](../public-api.md)),
  TS SDK (`web/shared`, `web/sdk`) with CI drift check, and `web/backend`
  (`@pohunek/relay`): a pure 1:1 WS↔daemon tunnel with fail-closed NetBird
  binding, tested against the SDK test matrix.
- **Track D** — `crates/gui-core` proved the "aggregation is client-side" model:
  headless multi-host state, per-host error markers, event reduction, fully
  unit-testable without a UI toolkit.

## Decisions

### Locked earlier (Phase 4 / ROADMAP) — reused as-is

- **Chassis unchanged.** No daemon network surface, no embedded assets, no
  daemon-side auth or TLS. The backend is a client of the public protocol.
- **Serve from the mesh.** The backend binds only to its host's NetBird address
  (fail-closed `validate_netbird_bind_addr` twin, already implemented in
  `web/backend/src/bind.ts`); loopback only with an explicit dev flag; never
  `0.0.0.0`.
- **Auth deferred.** Trust boundary is NetBird/WireGuard + filesystem
  permissions, identical to the daemon's own TCP listener. Bearer token +
  `Origin` allow-list is the recorded first hardening step if ever needed.
- **Frontend stack:** Svelte 5 (runes) + Vite; `@xterm/xterm` (+ fit and WebGL
  addons) for the terminal. Confirmed 2026-07-22.
- **Bun** runtime and workspace; Node stays the documented fallback for the
  SDK's `node:net` code paths.
- **Provider seam lives in the backend** (Slice E): `WorkItemProvider` (Linear
  via GraphQL) and `CodeHostProvider` (GitHub via `gh` subprocess), sharing the
  prompt-template + opaque `link.*` metadata conventions with the sway scripts
  and the native GUI.
- **PWA needs one ordinary TLS cert** on the backend host (Slice D); desktop
  works over plain HTTP + `ws://`.

### Made by this plan (2026-07-22, with the operator)

1. **Hybrid architecture — aggregation in the browser, thin backend.**
   The Phase 4 sentence "the browser uses one WebSocket for control + events"
   is superseded. The data path stays the **pure relay tunnel**:
   the SPA opens one control WS per host (`/daemon/<host>/control`) and one WS
   per attach (`/daemon/<host>/attach`) using the **unchanged `@pohunek/sdk`
   WebSocket transport** — the browser speaks the public protocol verbatim, so
   there is no second protocol surface and nothing new that can drift.
   Multi-host aggregation (connect fan-out, event reduction, session and
   notification stores, per-host error markers) lives in a new headless
   package **`web/client-core`** — the TS twin of `crates/gui-core`.
   The backend keeps a small **HTTP API** for what a browser cannot do itself:
   `GET /api/hosts` (host discovery), SPA static serving, and (Slice E) the
   provider endpoints. Control/attach payloads never terminate in the backend.
2. **Host discovery: dynamically via the local daemon, no static config.**
   The backend requires a running `pohunekd` on its own host (default
   owner-only Unix socket, same path the CLI uses) and treats it as a hard
   dependency: **fail-fast at startup** when unreachable. It calls
   `host.discover` (plus its own `daemon.health`) to build the relay routing
   table and the `/api/hosts` payload — the local host is always entry one
   (routed over the Unix socket), discovered peers are TCP targets. The table
   refreshes on an interval; a vanished host is dropped from routing but
   surfaces in `/api/hosts` history as unreachable. There is no host config
   file to maintain.
3. **Package layout.** `web/backend` grows from the bare relay into the
   control-center backend and is renamed **`@pohunek/backend`** (`relay.ts`
   stays as its transport module; the WS framing contract in
   `docs/public-api.md` is unchanged, only the package name ripples). New
   packages: **`@pohunek/client-core`** (`web/client-core`, headless state),
   **`@pohunek/frontend`** (`web/frontend`, Svelte SPA),
   **`@pohunek/testkit`** (`web/testkit`, fixture daemon). Pre-1.0, no rename
   shims.
4. **Mocks: a shared stateful fixture daemon + env-gated real-daemon e2e.**
   `web/sdk/test/mock-daemon.ts` (scripted exchanges) stays as the SDK's
   private unit-test tool. `@pohunek/testkit` adds a **stateful** fixture
   daemon: Unix + TCP listener speaking the real framing, an in-memory session
   registry, scenario controls (advance `agent_state`, emit notifications,
   drop a host), an echo PTY behind the attach stream, and `host.discover`
   fixtures. It is the one fake used by client-core unit tests, backend
   integration tests, the SPA dev mode, and Playwright e2e. On top of that,
   the real-daemon e2e pattern from the SDK (`POHUNEK_E2E=1`, spawned
   `pohunekd` in an isolated temp dir) extends to the backend + SPA level.
5. **Secrets: env file with 0600 permissions.** Provider credentials (the
   Linear API token, Slice E) live in `~/.config/pohunek/backend.env`
   (mode `0600`), loaded via systemd `EnvironmentFile=` (the unit ships with
   the backend) or sourced in dev. Rules: the variable is read **by name**
   (`LINEAR_API_TOKEN`) and required config **fails fast** when missing — no
   silent defaults; the token never reaches the browser, any daemon state,
   session metadata, events, or logs; Slice E tests assert secret-free logs
   and a secret-free browser-facing API. GitHub needs no token handling at
   all — `gh` owns its own auth. The daemon store only ever receives opaque
   `link.*` metadata.
6. **Milestone 1 feature set = Slice B + Slice C + notifications inbox.**
   Hosts + sessions workspace with live agent-state badges, session lifecycle
   (`session.new` / `inspect` / `stop`), the **in-browser terminal**
   (xterm.js over the attach WS, with `resize`/`detach` on control), and a
   notifications inbox (`notification.list`, live `notification` events,
   `notification.update` for read/acknowledge/archive). Everything else
   (projects, prompts, diff review, resume/fork/remove) is later-milestone
   parity work.

## Architecture

```
browser (Svelte SPA = @pohunek/frontend)
  │  renders stores from
  ▼
@pohunek/client-core        headless: connect fan-out, handshake + version
  │                         check per host, session/notification stores,
  │                         event reducer, reconnect, error markers
  │  uses @pohunek/sdk WS transport (public protocol, verbatim)
  ▼
@pohunek/backend (one origin, NetBird-bound)
  ├─ static SPA bundle (/)
  ├─ GET /api/hosts          ← host.discover + daemon.health via local daemon
  ├─ /daemon/<host>/control  ← 1:1 relay tunnel (text frames = control lines)
  └─ /daemon/<host>/attach   ← 1:1 relay tunnel (binary frames = raw bytes)
        │ node:net / Bun sockets
        ▼
   pohunekd on each host (Unix socket locally, NetBird TCP remotely)
```

The backend holds no authoritative state and no protocol logic on the data
path. If it is down, the CLI, GUI, and sway loop are unaffected.

Protocol-version skew is handled per host in client-core: each host handshake
checks strict equality against the SDK's `PROTOCOL_VERSION`; a mismatched host
is shown with an error marker, it never poisons the workspace.

## Milestones

- **M1 — backend + client-core + SPA (Slices B + C + notifications).**
  Specified end-to-end in `NEXT.md` (2026-07-22). Done when the Phase 4
  Slice B and C checks pass against ≥2 fixture daemons in CI, plus the
  notifications inbox, plus env-gated real-daemon e2e. A responsive mobile
  browser layout and touch terminal controls landed as an M1 follow-up.
- **M2 — Slice D: TLS + mobile PWA.** HTTPS/`wss://` with one operator-provided
  cert, web app manifest + service worker, install flow verified on a mobile
  browser; the existing responsive browser and plain-HTTP desktop paths remain
  unchanged.
- **M3 — Slice E: providers (Linear + GitHub).** Backend provider seam and
  `/api/providers/*` endpoints; browse issues/PRs; launch-on-item via
  `session.new` with rendered prompt templates (`~/.config/pohunek/prompts/`,
  shared conventions); opaque `link.*` metadata byte-identical with the sway
  scripts and the GUI; PR checks/review status beside the state badge;
  worktree diff via `session.diff`. Secrets per decision 5, with
  leak-assertion tests.
- **M-later — parity backlog.** Projects/worktrees management, prompt
  management, session resume/fork/remove/rename in the UI, diff review
  surface, notification policy editing.

## Risks

- **Many WebSockets from the browser** (one control WS per host + one per
  attach). Fine on desktop; on mobile (M2) connection count and battery are
  the price of the no-second-protocol design — accepted; the relay is
  transparent so a future aggregating endpoint could be added without
  touching the SPA's client-core API if it ever hurts.
- **Hard dependency on the local daemon** for discovery: the backend refuses
  to start without it. Deliberate (zero config); the failure message names
  the socket path and the fix.
- **Attach bridging under load** — already de-risked: the relay's
  backpressure handling is bounded and tested from the S.3 milestone; xterm.js
  adds only a renderer on top.
- **Svelte 5 + xterm.js integration** is new code paths for this repo —
  covered by Playwright e2e in CI from day one, not manual testing.
- **Fixture-daemon drift from the real daemon.** Mitigated by building the
  testkit on `@pohunek/protocol` generated types and keeping the env-gated
  real-daemon e2e in CI as the honesty check.

## Done criteria (Track B)

- From a browser on the mesh: one origin shows every host and session with
  live agent-state, full session lifecycle, in-browser terminal attach/detach,
  and the notifications inbox (M1).
- Installable PWA over one ordinary cert; desktop unchanged over HTTP (M2).
- Browse/launch/link/review on Linear + GitHub with credentials only in the
  backend env file; links byte-identical with the other surfaces (M3).
- The daemon is byte-for-byte unchanged across the whole track, and CLI/GUI
  keep working with the backend down.
