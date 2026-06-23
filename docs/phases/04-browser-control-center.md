# Phase 4: Browser Control Center

## Objective

Keep `pohunek` a **chassis** (a headless engine with a stable control protocol)
and add a **browser-based control center** on top of it — but route the browser
through a **standalone TypeScript aggregator backend**, not through the daemon.
The backend is a long-lived client of the existing control protocol: it dials
each host's daemon exactly the way the CLI does (native newline-delimited JSON
over NetBird TCP, plus the separate attach byte stream), fans those out, and
serves a single SPA + WebSocket API to the browser. The daemon gains **no new
network surface**.

This phase **supersedes Phase 3's libghostty native-GUI direction** (the native
GUI is dropped in favor of a browser app) and **absorbs Phase 3's provider track**
(Later A) into the control center backend.

## User Value

From any device on your NetBird network — laptop or phone — open a browser, see
every host and every agent session at once, watch live agent state, attach to a
session's terminal, **browse your Linear issues and GitHub PRs, launch an agent
straight onto one with a preset prompt, and review the resulting diff / PR**.
This is the browser twin of the Phase 5 sway launcher and a Kandev-style
work-item / code-host workspace — same engine, no native install per platform, no
per-daemon browser plumbing.

## Why a standalone aggregator backend (and not a daemon gateway)

A browser cannot speak the daemon's raw TCP control protocol. The earlier
direction solved this by adding an HTTP + WebSocket gateway **inside every
daemon** and having the browser connect directly to N daemons. That pushed a
large amount of browser-facing complexity into the Rust daemon (a second network
surface, embedded static assets, per-daemon TLS).

Routing through a standalone backend collapses that:

- The backend is an ordinary process, so — unlike a browser — it **can** open raw
  TCP connections to each daemon. It speaks the **existing, already-tested**
  native protocol as a client (like the CLI / remote client does). No new wire
  format, no gateway in Rust.
- All browser-facing concerns (HTTP, WebSocket, serving the SPA, TLS, any future
  auth) live in TypeScript, where they are natural — and in **one** place, not in
  every daemon.
- The browser opens connections to a **single origin** (the backend), so the
  per-daemon trust/cert story disappears entirely.

The cost is explicit and accepted: this is a **central aggregator**. See
"Design principles" and "Risks".

## Design principles

- **Chassis unchanged.** `pohunek` (daemon + protocol) owns sessions, PTYs,
  worktrees, and host state, and exposes the same control protocol it does today.
  Phase 4 adds **no** daemon network surface, **no** embedded assets, **no** auth
  in the daemon. The daemon stays provider-agnostic and presentation-agnostic.
- **Aggregator is a client, not an authority.** The backend holds **no**
  authoritative state. Each host's daemon remains the single source of truth for
  its own sessions; the backend only fans out (`host discover`, per-host
  `session list`), proxies attach streams, and renders. It is the
  client-side aggregation that Phase 2 / Milestone 12 already do in the CLI,
  promoted to a long-lived process.
- **The CLI path stays direct and independent.** The CLI and the rofi/sway loop
  (Phase 5) continue to dial daemons directly over NetBird. If the backend is
  down, the core tool is unaffected — the UI is not on the critical path.
- **Trust is the network (for now).** The trust boundary is **NetBird/WireGuard +
  filesystem permissions**, identical to the daemon's own model: the daemon's TCP
  listener already trusts the mesh and has no application-level auth. The backend
  runs **inside** the mesh on a NetBird address, so it inherits the same boundary.
  Application-level auth is **deferred** (see Decisions); it can be added in the
  backend later without touching the daemon or the protocol.
- **Serve from the mesh.** The backend binds to its host's NetBird (CGNAT)
  address. A page served from a NetBird IP, talking WS to the same NetBird origin,
  stays in the browser's "private → private" lane (see Browser platform
  constraints) — no Local Network Access prompt, no preflight.

## Scope

- **Public API + SDKs (chassis prep).** Promote the control protocol from an
  internal CLI wire format to a documented, versioned **public** API, shipped as
  two SDKs: a **Rust SDK** (`crates/client`, extracted from the CLI's existing
  client) and a **TS SDK** (`web/sdk` — `ts-rs` types + a pluggable TCP/WebSocket
  transport). Our own clients (CLI, aggregator backend, browser) are the SDKs'
  first consumers; anyone can build their own client on the same SDKs.
- **Standalone aggregator backend (TS).** A long-lived Node/Bun process in a
  `web/` workspace that: connects to N daemons over the native NetBird TCP
  protocol; fans out discovery, session listing, lifecycle, and event
  subscriptions; bridges each daemon attach byte stream to a browser WebSocket;
  and serves the SPA + a single browser-facing HTTP/WS API. **No daemon changes.**
- **Browser control center (SPA).** A web client that talks **only** to the
  backend, renders hosts/sessions/state, attaches a terminal in the browser
  (xterm.js), and drives the full session lifecycle.
- **Provider integration in the backend (Linear + GitHub), Kandev-style.** A
  backend-side **provider seam** (adapter model, Linear / GitHub first) that
  browses work items and PRs, **launches agents on an issue / PR** with a preset
  prompt (the browser twin of the Phase 5 sway launcher), links sessions to work
  items / PRs, and surfaces PR / issue status and worktree diffs. GitHub goes
  through `gh`; Linear through its GraphQL API. Links are stored
  as **opaque metadata** in the daemon store via the existing protocol — the
  **same store** the sway scripts use, so links are shared across both surfaces.
  Provider credentials live only in the backend, never in daemon state or the
  event log.
- **Optional single TLS cert for mobile PWA.** One ordinary certificate on the
  backend host (not a per-daemon mesh CA) when a secure context / installable PWA
  is wanted on mobile.

## Out of Scope

- Native / desktop GUI (libghostty, GTK, Electron-native) — explicitly dropped.
  (A Tauri desktop client that speaks the native protocol directly is a possible
  *later, optional* addition; it does not serve the mobile goal and is not built
  here.)
- A daemon-side HTTP/WS gateway, embedded GUI bundle, or per-daemon mesh CA — all
  removed from this phase by the aggregator model.
- A mandatory SaaS / public-origin dashboard. The backend is a user-run process
  on a host inside the NetBird mesh; a public origin would break the
  private→private browser lane.
- Application-level auth and multi-user authorization / RBAC — deferred while the
  trust boundary is the VPN (single operator, single trusted network).
- In-tree provider API adapters in the chassis (providers stay shell-out / MCP in
  the backend).
- New session semantics — the lifecycle and protocol from Phases 1–2 are reused.

## Browser platform constraints (verified)

In the aggregator model the browser talks to **one** origin (the backend), so the
per-daemon constraints from the earlier direction no longer apply. What remains
load-bearing (verified against vendor docs / specs, current as of mid-2026):

- **Local Network Access (Chrome 142/147+) does NOT bite us.** LNA prompts and
  Private Network Access preflights only apply to **public → private**
  connections. The backend is served from a NetBird IP (CGNAT, `100.64.0.0/10`,
  classified as private) and the browser is on a NetBird IP, so browser → backend
  is private → private — exempt. This is why "serve from the mesh" is a hard rule:
  a public-origin backend would re-enter prompts/preflights. The backend → daemon
  connections are **server-side TCP** and not subject to any browser rule.
- **Secure context / PWA needs one real cert.** Desktop works over plain HTTP +
  `ws://` on the NetBird address. For an **installable PWA / service worker** on
  mobile, the page must be a secure context, which needs HTTPS — but only **one**
  ordinary certificate on the backend host, not a mesh CA across every daemon.
  (Top-level navigation to a self-signed cert can be click-through-accepted on
  mobile; PWA install requires a properly trusted cert.)
- **WebSocket has no CORS preflight.** The browser sends `Origin` on the upgrade
  but does not enforce CORS for WS. While auth is deferred, the backend is treated
  exactly like the daemon's TCP port today: reachable only inside the mesh, trust
  delegated to NetBird policy. An `Origin` allow-list (and a token) is the natural
  first hardening step **if** the backend ever needs to be trusted beyond the
  mesh — recorded under Decisions, not built now.

## Slices and Definition of Done (testable)

The phase is delivered in slices, each independently valuable. The phase is
**not done** until A–C hold (the browser control center works multi-host on
desktop); D and E complete the mobile and provider goals.

### Slice A — Public API contract + Rust SDK + TS SDK (chassis prep)

The protocol is consumed only through **SDKs**, never hand-rolled wire code: our
own clients (CLI, aggregator backend, browser) build on them, and so can any
third-party client.

1. The control protocol (methods, envelopes, error classes/codes, events, **and
   the attach byte stream**) is documented as a **versioned public API**;
   protocol-version negotiation already governs skew.
2. **Rust SDK** (`crates/client`, on the existing `crates/protocol` types): the
   transport-agnostic client that today lives `pub(crate)` in
   `crates/cli/src/client.rs` is extracted into a reusable crate — connect (Unix
   socket / NetBird TCP), request/response, event subscription, and the **attach
   duplex stream** — with its own error type. The CLI becomes a **consumer** of
   the SDK with no behavior change. This extraction is low-risk and may land as a
   standalone refactor before the rest of Phase 4.
3. **TS SDK** (`web/sdk`): the `ts-rs`-generated types (`web/shared`) plus a
   runtime client with a **pluggable transport** — a **TCP** transport (Node/Bun →
   daemon directly, like the Rust SDK) and a **WebSocket** transport (browser →
   aggregator backend, since browsers cannot open raw TCP). Same methods and types
   over either transport; attach is exposed as a duplex byte stream.
4. A CI **drift check** fails if the generated TS types diverge from the Rust
   protocol source.
   *Check:* a minimal client built only on each SDK performs `daemon.health` and
   `session.list`, subscribes to an event, and round-trips an attach stream — the
   Rust SDK and the Node/Bun TS SDK **directly against a daemon**, the browser TS
   SDK **through the backend** — with no hand-written wire types.

### Slice B — Standalone aggregator backend (no daemon changes)

3. A long-lived TS backend (in the `web/` workspace) connects to **N daemons**
   over the native NetBird TCP control protocol, **built on the TS SDK (TCP
   transport)**. It enumerates hosts (`host discover`), runs per-host
   `session list` **concurrently** (short timeout; partial results with a per-host
   error marker on failure — same semantics as the rofi switcher), and drives the
   full lifecycle (new / list / inspect / stop) by relaying control requests.
4. The backend **bridges the separate attach byte stream**: it opens the daemon's
   raw attach connection (control `attach` → `stream_id` → second connection) and
   pipes it bidirectionally to a browser WebSocket, so terminal output flows down
   and keystrokes flow up. `resize` / `detach` are relayed on the control
   connection. The PTY stays owned by the daemon; detach does not kill the remote
   process.
5. The backend serves the static SPA bundle and a single browser-facing HTTP/WS
   API, bound only to the host's NetBird address (reusing the Phase 2 fail-closed
   `validate_netbird_bind_addr` semantics — **never** `0.0.0.0`; local-only when
   NetBird is absent). The **daemon is unchanged**: no gateway, no embedded
   assets, no daemon-side auth.
   *Check (CI, no real mesh):* against ≥2 loopback-TCP stand-in daemons, the
   backend lists both hosts' sessions, surfaces a state change from the event
   subscription, relays a `session.stop`, and round-trips an attach byte stream
   end-to-end (daemon TCP ↔ backend ↔ a WS test client); a down host yields a
   marked partial result, not a hang.

### Slice C — Browser control center (talks only to the backend)

6. A TypeScript SPA connects to the **backend** (one origin) **via the TS SDK
   (WebSocket transport)** and renders a unified workspace: hosts, sessions, and
   **live agent-state badges** driven by the
   backend's relay of the existing event subscription (`agent_state`,
   `session_created/updated/stopped`, `attach_opened/closed`).
7. The app drives the full lifecycle (new / list / inspect / stop) and **attaches
   a terminal in the browser** (xterm.js) over the backend's WS attach bridge;
   detach leaves the session running on its host.
   *Check:* against the Slice B backend over ≥2 loopback-TCP stand-in daemons
   (CI), the app lists both hosts' sessions, shows a state change as an event,
   attaches and round-trips terminal I/O, and detaches leaving the session
   running. Desktop tier uses plain HTTP + `ws://` on the backend's NetBird
   address.

### Slice D — Single TLS cert for mobile / PWA (optional)

8. The backend can serve **HTTPS + `wss://`** with **one ordinary certificate**
   for its NetBird host (obtained however the operator prefers; no per-daemon mesh
   CA is introduced).
9. With that cert trusted by the device, the GUI is a **secure context** and the
   control center is **installable as a PWA** on mobile; desktop continues to work
   on plain HTTP without it.
   *Check:* with the backend served over `wss://` with a trusted cert, a mobile
   browser loads the control center, attaches a terminal, and installs the PWA;
   on plain HTTP the desktop path still works and PWA install is (expectedly)
   unavailable.

### Slice E — Provider integration in the backend (Linear + GitHub: browse, launch, link, review)

The backend gains a **provider seam** — Kandev's adapter model, but living in the
backend, never in the chassis: a `WorkItemProvider` (Linear-first; Jira / GitHub
Issues addable behind the same interface) and a `CodeHostProvider` (GitHub-first
via `gh`; GitLab / Forgejo later). Each adapter is shell-out / API behind a thin
interface — the **same seam** the Phase 5 launcher scripts use — so the chassis
stays provider-agnostic. This slice is the browser-surface twin of the Phase 5
sway launcher: the same actions, exposed visually instead of via rofi + scripts,
sharing one source of truth.

10. **Browse work items and PRs.** The UI lists the operator's Linear issues and
    GitHub PRs / issues (assigned / authored / filterable) in the workspace,
    fetched by the backend's provider adapters.
11. **Launch an agent on an item** (parity with `pohunek-launch-issue` /
    `pohunek-launch-pr`). From an issue or PR, the backend derives context
    (title / body / branch), renders a **preset prompt from the same templates the
    sway scripts use** (`~/.config/pohunek/prompts/*.tmpl`, `${var}`
    substitution), resolves agent / host / repo from config defaults, and starts
    the session **atomically** via `session new --branch --input <rendered>` over
    the control protocol (the Phase 5 Slice B path). The worktree on the item's
    branch is created by the existing session / worktree machinery.
12. **Link existing sessions, shared across surfaces.** A work-item / PR link is
    stored as an **opaque metadata `kind`** in the daemon store via the protocol;
    the chassis never interprets it. Because this is the **same store** the sway
    scripts write, a session launched-and-linked from sway shows its link in the
    browser and vice versa — **one source of truth, two clients**.
13. **Review-first surface.** The UI shows the linked issue state and the PR's
    checks / review status next to the session's live agent-state badge, renders
    the worktree **diff** in the browser, and can **open / view a PR**
    (`gh pr create` / `gh pr view`) from the session's worktree.

Provider credentials live **only** in the backend (gh's own auth; a Linear API
token), never in daemon state, session metadata, or the event log.

*Check:* with fixture providers, the UI lists issues / PRs; launching on a fixture
issue starts exactly one session on the expected branch with the rendered prompt
delivered as input; the link persists across daemon restart and is byte-identical
to a link written by the Phase 5 script (proving the shared opaque `kind`); the
daemon treats the link as opaque; and no provider token appears in any daemon log,
session-metadata record, or event.

## Architecture Impact

- The control protocol becomes a **first-class public API** consumed only through
  the **SDKs** (Rust `crates/client` + TS `web/sdk`), never hand-rolled wire code:
  the CLI, the aggregator backend, and the browser all build on them, and so can
  any third-party client. PTY/worktree/session ownership stays in the daemon,
  **unchanged**. The transport split is load-bearing: Rust/Node/Bun clients reach
  daemons directly (TCP/Unix); browsers reach them only through the backend (WS).
- The daemon gains **no** new surface. All browser-facing code — HTTP, WS, SPA
  serving, attach bridging, TLS, future auth — lives in the standalone backend.
- The backend is a **client/aggregator, not an authority**. Each host remains
  authoritative; the backend holds no replicated state. This is the
  single-aggregator deployment that earlier docs recorded as a "fallback,"
  promoted to the **primary** UI model — accepted deliberately for the daemon
  simplicity and single-origin browser story it buys.
- The metadata store gains a new opaque `kind` for provider links; no new
  authoritative state leaves the owning host. Because the link is opaque metadata
  in the daemon store, the Phase 5 sway launcher and the browser backend read and
  write the **same** links — one source of truth, two client surfaces — and the
  provider adapter model (Kandev-style) lives entirely in the backend, not the
  chassis.

## Risks

- **Central aggregator = single point of failure / compromise for the UI.**
  Mitigation: the backend holds no authoritative state and the CLI + rofi/sway
  loop stay direct, so a backend outage degrades only the browser UI, not the
  tool. The backend runs inside the mesh on a host you own; its blast radius
  (network reach to your daemons) is no larger than the CLI already has from any
  mesh host.
- **No application auth yet.** Accepted while the trust boundary is the VPN —
  identical to the daemon's own TCP listener today. Mitigation/path: a token +
  `Origin` allow-list can be added in the backend, in one place, without daemon or
  protocol changes, the moment the backend needs trust beyond the mesh.
- **Attach-stream bridging is real work.** Piping the daemon's separate raw-byte
  attach connection to a browser WS (and relaying resize/detach on control) is the
  trickiest part of the backend. Mitigation: it reuses the exact attach protocol
  the CLI already drives; covered by the Slice B end-to-end byte-stream test.
- **Bun's raw-TCP + byte-stream backpressure.** The backend's hot path (TCP to
  daemon ↔ WS to browser under load) is the one place Bun is less battle-tested
  than Node. Mitigation: a **spike at the start of Slice B** that round-trips a
  high-volume attach stream; Node is a drop-in fallback on the same `net` API if
  it disappoints.
- **TS/Rust type drift.** Mitigation: generate TS from Rust + a CI drift check
  (Slice A); never hand-maintain wire types.
- **Provider credential leakage.** Mitigation: creds live only in the backend;
  daemon stores opaque links; the event log is asserted secret-free.
- **Provider logic duplicated between the sway scripts and the backend.**
  Mitigation: share the launch primitive and conventions (prompt templates, the
  opaque link `kind`, `session new --input`); the daemon store is the single
  source of truth for links, so the two surfaces cannot disagree about what a
  session is linked to.
- **Community Linear tooling churn** (no official first-party Linear CLI, as in
  Phase 5). Mitigation: keep Linear behind the `WorkItemProvider` seam so the
  backing tool (community CLI / GraphQL / MCP) can be swapped without touching the
  UI or the chassis.

## Success Criteria

- Open a browser on a laptop or phone on the NetBird network and see every host
  and session in one workspace, with live agent-state — by connecting to a single
  backend.
- The daemon is byte-for-byte the same as Phase 2/3: no gateway, no embedded GUI,
  no daemon-side auth or certs.
- Attach a session's terminal in the browser and detach without killing the
  remote process.
- Browse Linear issues / GitHub PRs in the workspace, launch an agent onto one
  with a preset prompt, link sessions, and review the diff / PR — with provider
  credentials only in the backend, and links shared with the Phase 5 sway
  launcher via the daemon store.
- Mobile works as an installable PWA once the backend has a single trusted TLS
  cert; desktop works immediately over plain HTTP `ws://`.
- The CLI and rofi/sway loop keep working directly against daemons even when the
  backend is down.

## Decisions (resolved)

- **Topology.** A **standalone TS aggregator backend** is the primary (and only
  built) UI model. The browser talks only to the backend; the backend talks to
  daemons as a native protocol client. The daemon-embedded-gateway + direct
  browser fan-out direction is **dropped**.
- **Auth.** **Deferred.** The trust boundary is NetBird/WireGuard + filesystem
  permissions, the same model the daemon's TCP listener already relies on; the
  backend runs inside the mesh on a NetBird address. A bearer token + `Origin`
  allow-list is the recorded first hardening step if the backend ever needs trust
  beyond the mesh — addable in the backend alone, no daemon/protocol change.
- **TLS.** Desktop runs plain HTTP + `ws://` on the NetBird address. Mobile PWA
  needs **one ordinary cert on the backend host** (Slice D), not a per-daemon mesh
  CA. The mesh-CA approach is **dropped**.
- **SDKs.** The protocol ships as a **Rust SDK** (`crates/client`) and a **TS SDK**
  (`web/sdk`). The Rust SDK is an **extraction** of the CLI's existing
  `pub(crate)` client and can land as a standalone refactor before the rest of
  Phase 4. The TS SDK has **two transports** — TCP (Node/Bun → daemon direct) and
  WebSocket (browser → backend) — because browsers cannot open raw TCP. Both are
  structured as **publishable** packages, but publishing to crates.io / npm and
  any public stability promise are **deferred** until the protocol settles in
  daily use (see API stability). The attach byte stream is a first-class SDK
  primitive (a duplex stream), not an afterthought.
- **API stability.** **No compatibility promise yet:** the protocol is documented
  and version-negotiated. A public SDK plus a second independent client (the
  browser, alongside the CLI) is exactly the condition for committing stability,
  but the promise stays **deferred pre-1.0**: SDK semver tracks the protocol
  version, generated TS types track the Rust source via the CI drift check, and
  breaking changes are allowed (with a version bump) until the promise is made.
- **Tech stack (locked).**
  - **Backend:** **Bun** runtime (TCP client to daemons via `Bun.connect`, native
    WebSocket server, `gh` as subprocess). Node is the no-regret fallback (same
    `net` API) if Bun's TCP/stream backpressure disappoints — see the spike risk.
  - **SPA:** **Svelte 5** (runes) built with **Vite**; **xterm.js** + fit addon +
    webgl renderer for the terminal. Served by the backend (no `include_dir!` into
    the daemon).
  - **Type generation:** **`ts-rs`** — TS types derived directly from the Rust
    protocol structs into `web/shared`; protocol version negotiation governs
    compatibility (no separate runtime validator for now).
  - **Workspace:** **Bun workspaces** — `web/sdk` (TS SDK: types + transports),
    `web/shared` (ts-rs generated types, consumed by the SDK), `web/backend`,
    `web/frontend`. `strict: true`, ESLint, explicit return types.
  - **Linear:** **GraphQL API directly** (token) from the backend — no third-party
    CLI dependency. GitHub via `gh`. The `WorkItemProvider` seam still lets the
    Phase 5 scripts keep their community CLI; only the backing differs per surface.
- **Browser-facing protocol & attach fan-out.** The browser uses **one WebSocket**
  for control requests + the event subscription (JSON frames) and **one WebSocket
  per attach** (binary frames). For a session viewed by two browsers, the backend
  opens **one daemon attach connection per browser** (1:1 transparent
  pass-through) and lets the daemon handle multi-attach / resize as it already
  does — the backend does not buffer or multiplex a shared stream.
- **Backend deployment.** The backend runs as a **systemd user service** (like the
  daemon) on one always-on host **inside** the NetBird mesh, bound to that host's
  NetBird address.
- **Provider model.** Kandev's adapter model, but in the backend: a
  `WorkItemProvider` (Linear-first; Jira / GitHub Issues later) and a
  `CodeHostProvider` (GitHub-first via `gh`; GitLab / Forgejo later), each behind
  a thin seam so other trackers / forges can be added without chassis changes. The
  backend's launch-on-item flow is the **browser twin of the Phase 5 launcher
  scripts** and reuses the same conventions — prompt templates in
  `~/.config/pohunek/prompts/`, the `session new --input` atomic launch, and
  opaque link metadata in the daemon store. Where practical it shares the launch
  primitive with the scripts rather than reimplementing it, so the two surfaces do
  not drift. The chassis stays provider-agnostic.
- **Tauri.** Not built in this phase. Recorded as a possible later, optional
  desktop client that could speak the native TCP protocol directly (no backend,
  no browser constraints) — it does not address the mobile-from-browser goal.

## Exit Criteria

- The control protocol is a documented, versioned public API consumed through a
  **Rust SDK** (`crates/client`) and a **TS SDK** (`web/sdk`, TCP + WebSocket
  transports) with drift-checked, ts-rs-generated types — usable by third-party
  clients, not just our own.
- A standalone TS aggregator backend connects to N daemons over the native
  protocol, bridges control + attach over a single browser-facing HTTP/WS API,
  bound only to NetBird — with the daemon unchanged.
- The browser control center drives the full multi-host session lifecycle and
  in-browser attach on desktop, and on mobile as a PWA via a single backend cert.
- The browser UI browses Linear issues / GitHub PRs, launches an agent onto an
  item with a preset prompt (parity with the Phase 5 launcher), links sessions,
  and reviews the diff / PR — all in the backend, with links stored opaquely in
  the chassis and shared with the sway launcher.
- No native GUI, no daemon-side gateway, and no per-daemon mesh CA were
  introduced.
