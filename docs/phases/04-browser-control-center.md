# Phase 4: Browser Control Center

## Objective

Turn `pohunek` into a **chassis** (a headless engine with a stable public API)
and add a **browser-based control center** on top of it. The control center is a
pure client: it talks **directly** to every host's daemon over NetBird, presents
a unified multi-host workspace, attaches to sessions in the browser, and is where
provider integrations (GitHub / Linear) live. No native/desktop GUI is built —
the client is a web application that also runs on mobile.

This phase **supersedes Phase 3's libghostty native-GUI direction** (the native
GUI is dropped in favor of a browser app) and **absorbs Phase 3's provider track**
(Later A) into the control center.

## User Value

From any device on your NetBird network — laptop or phone — open a browser, see
every host and every agent session at once, watch live agent state, attach to a
session's terminal, and link work to GitHub PRs / Linear issues. Same engine, no
native install per platform, no central server, no cloud API token for the mesh
itself.

## Design principles

- **Chassis vs. control plane.** `pohunek` (daemon + protocol) owns sessions,
  PTYs, worktrees, and host state, and exposes a stable API. Everything
  provider-aware and presentational lives in the control center (a client). The
  daemon stays provider-agnostic.
- **Direct, not centralized.** The browser client connects directly to each
  daemon over NetBird. There is no mandatory aggregation hub — each host stays
  authoritative for itself, consistent with Phase 2. A single-aggregator endpoint
  exists only as an optional fallback (see Slice D).
- **Serve from the mesh.** The GUI is served by a daemon on its NetBird (CGNAT)
  address, never from a public origin. This keeps browser connections in the
  "private → private" lane (see Browser platform constraints).
- **Auth is mandatory the moment a browser is involved.** The control protocol
  has no application-level auth today (it trusts the transport). A browser-facing
  surface drives a code-executing daemon, so it requires a token + Origin
  allow-list from day one.

## Scope

- **Public API contract.** Promote the control protocol from an internal CLI wire
  format to a documented, versioned **public** API, with **TypeScript types
  generated from the Rust protocol types** so clients never hand-roll the wire
  format.
- **Daemon GUI gateway (opt-in).** An HTTP + WebSocket server, *off by default*,
  bound only to the host's NetBird address. Serves the embedded static GUI bundle
  and bridges the browser to the control protocol and the attach byte stream.
- **Browser control center (TS).** A multi-host web client that connects directly
  to N daemons, renders hosts/sessions/state, attaches a terminal in the browser,
  and drives the full session lifecycle.
- **Mesh TLS for mobile/PWA.** A `pohunek`-issued mesh CA so each daemon's
  `wss://` endpoint is browser-trusted after a one-time per-device root-CA install
  — required because self-signed certs are rejected for WebSocket on mobile.
- **Provider integration in the control center.** GitHub via `gh`, Linear via its
  MCP/API; session ↔ work-item/PR links stored as **opaque metadata** in the
  daemon store. Provider credentials live only in the control center.

## Out of Scope

- Native / desktop GUI (libghostty, GTK, Electron-native) — explicitly dropped.
- A mandatory central server or hosted SaaS dashboard (the control center is a
  user-run client; a public-origin host is a non-goal and would break the
  private→private browser lane).
- Multi-user authorization / RBAC (single operator; the token is a single bearer).
- In-tree provider API adapters in the chassis (providers stay shell-out / MCP in
  the control center).
- New session semantics — the lifecycle and protocol from Phases 1–2 are reused.

## Browser platform constraints (verified)

These facts shaped the design and are load-bearing for implementers (verified
against vendor docs / specs, current as of mid-2026):

- **Local Network Access (Chrome 142/147+) does NOT bite us.** LNA prompts and
  Private Network Access preflights only apply to **public → private**
  connections. NetBird IPs are CGNAT (`100.64.0.0/10`), classified as private. A
  page **served from a NetBird IP** connecting to **other NetBird IPs** is
  private → private, which is exempt — no permission prompt, no preflight. Safari
  doesn't enforce LNA; Firefox doesn't enforce it for WebSocket by default. This
  is why "serve from the mesh" is a hard design rule: a public-origin GUI would
  re-enter prompts/preflights.
- **WebSocket has no CORS preflight — the server MUST validate `Origin`.** The
  browser sends `Origin` on the upgrade but does not enforce CORS for WS, so a
  cross-site page can open a WS to the daemon (Cross-Site WebSocket Hijacking).
  The daemon's Origin allow-list is the only defense and is mandatory.
- **Mixed content / secure context.** An HTTPS page can only open `wss://` (not
  `ws://`). A plain-HTTP page may use `ws://` but is **not a secure context** (no
  PWA / service worker / push / camera). This forces the two TLS tiers below.
- **Self-signed certs fail for WebSocket on mobile.** iOS Safari and Android
  Chrome reject self-signed certs for `wss://` with no click-through; desktop has
  no override for sub-resource WS either. → a real trust chain (a mesh CA, or
  per-device root install) is required for mobile, not optional.
- **NetBird gives names but not trusted certs.** NetBird magic DNS provides
  `host.netbird.cloud` names, but there is no built-in browser-trusted TLS for
  peer services (open upstream issue, no committed timeline). So the cert/trust
  story is `pohunek`'s responsibility (Slice D), not something to wait for.

## Slices and Definition of Done (testable)

The phase is delivered in slices, each independently valuable. The phase is
**not done** until A–C hold (the browser control center works multi-host on
desktop); D and E complete the mobile and provider goals.

### Slice A — Public API contract + generated TS types (chassis prep)

1. The control protocol (methods, envelopes, error classes/codes, events) is
   documented as a **versioned public API**; protocol-version negotiation already
   governs skew.
2. TypeScript type definitions are **generated from the Rust protocol types**
   (e.g. `schemars` → JSON Schema → TS, or `ts-rs`) and a CI check fails if the
   generated types drift from the Rust source.
   *Check:* a minimal TS client built only from the generated types can perform
   `daemon.health` and `session.list` against a daemon and parse the responses
   with no hand-written wire types.

### Slice B — Daemon GUI gateway + auth

3. An **opt-in** HTTP + WebSocket gateway (`pohunek gui serve` / daemon config
   flag), **off by default**, binds only to the host's NetBird address (reusing
   the Phase 2 fail-closed `validate_netbird_bind_addr`); it is **never** `0.0.0.0`
   and runs local-only / refuses to start wide when NetBird is absent.
4. The gateway serves the **embedded** static GUI bundle (compiled into the
   binary) and a WS endpoint that bridges to the control protocol **and** the
   separate attach byte stream.
5. Every HTTP/WS request requires the **mesh-wide bearer token** (one secret every
   daemon accepts, issued and rotatable via the CLI, stored owner-private) and
   passes an **`Origin` allow-list** check on the WS upgrade.
   *Check:* unit/integration tests assert: bind refuses a non-NetBird address; a
   request without a valid token is rejected; a WS upgrade with a disallowed
   `Origin` is rejected (CSWSH defense); a valid token + Origin round-trips a
   `session.list` and an attach stream over WS.

### Slice C — Browser control center, direct multi-host

6. A TypeScript web app connects **directly** to N daemons over NetBird (no
   aggregation hub), authenticates with each host's token, and renders a unified
   workspace: hosts, sessions, and **live agent-state badges** driven by the
   existing event subscription (`agent_state`, `session_created/updated/stopped`,
   `attach_opened/closed`).
7. The app drives the full lifecycle (new / list / inspect / stop) and **attaches
   a terminal in the browser** (e.g. xterm.js) over the WS attach bridge; the PTY
   stays owned by the daemon, and detach does not kill the remote process.
   *Check:* against ≥2 loopback-TCP stand-in daemons (CI), the app lists both
   hosts' sessions, shows a state change as an event, attaches and round-trips
   terminal I/O, and detaches leaving the session running. Desktop tier uses
   plain HTTP + `ws://` over the NetBird address.

### Slice D — Mesh TLS for mobile / PWA

8. `pohunek` can issue a **mesh root CA** and per-daemon certificates (SAN =
   the daemon's NetBird IP and/or its magic-DNS name); the gateway serves `wss://`
   with that cert.
9. After a **one-time per-device root-CA install** (documented for desktop and
   mobile), the browser trusts every daemon's `wss://` with no per-host friction,
   the GUI is a **secure context**, and the control center is **installable as a
   PWA** on mobile.
   *Check:* with the mesh root CA trusted, a mobile browser connects to ≥2
   daemons over `wss://` with no cert warning and installs the PWA; without it,
   the documented failure (cert rejection) is reproduced, justifying the CA.

### Slice E — Provider integration (control center)

10. The control center links sessions to GitHub PRs (via `gh`) and Linear issues
    (via MCP/API). Links are stored as an **opaque metadata `kind`** in the daemon
    store (the chassis never interprets them); provider credentials live **only**
    in the control center, never in daemon state or the event log.
    *Check:* linking a session to a PR/issue persists across daemon restart (it is
    in the store), the daemon treats the link as opaque, and no provider token
    appears in any daemon log or event record.

## Architecture Impact

- The control protocol becomes a **first-class public API** with generated client
  types; this is the contract the browser client (and any future client) builds
  on. PTY/worktree/session ownership stays in the daemon.
- The daemon gains a **second, opt-in network surface** (HTTP/WS gateway) distinct
  from the raw control port, with its own auth (token + Origin) — the first
  application-level auth in the system. Trust is no longer "the mesh"; it is "the
  mesh **and** a valid token from an allowed origin".
- The control center is a client/aggregator, **not** an authority. Each host
  remains authoritative. A single-aggregator deployment is an optional fallback
  for environments where a device root-CA install is impossible.
- The metadata store (Milestone 9) gains a new opaque `kind` for provider links;
  no new authoritative state leaves the owning host.

## Risks

- **Browser-facing daemon attack surface.** Mitigation: gateway off by default,
  bound only to NetBird, mandatory token + `Origin` allow-list, CSWSH tests; the
  raw control port is unchanged and separate.
- **Cert/trust friction on mobile.** Mitigation: the mesh CA (Slice D) and a
  documented one-time per-device install; desktop tier (Slice C) ships first on
  `ws://` and needs no certs.
- **TS/Rust type drift.** Mitigation: generate TS from Rust + a CI drift check
  (Slice A); never hand-maintain wire types.
- **Reintroducing a central server by accident.** Mitigation: direct fan-out is
  the primary model; the aggregator is a clearly-labeled fallback, not a
  requirement.
- **Provider credential leakage.** Mitigation: creds live only in the control
  center; daemon stores opaque links; the event log is asserted secret-free.
- **NetBird DNS on mobile.** Magic-DNS names may need nameserver config on mobile;
  Mitigation: certs also carry the NetBird IP SAN so IP-based `wss://` works
  without DNS.

## Success Criteria

- Open a browser on a laptop or phone on the NetBird network and see every host
  and session in one workspace, with live agent-state.
- Connect directly to each host's daemon — no central server, no aggregation hub
  required.
- Attach a session's terminal in the browser and detach without killing the
  remote process.
- Link a session to a GitHub PR or Linear issue, with credentials only in the
  control center.
- Mobile works as an installable PWA once the mesh root CA is trusted; desktop
  works immediately over `ws://`.
- The daemon's GUI surface is reachable only on the NetBird interface, only with a
  valid token, and only from an allowed origin.

## Decisions (resolved)

- **Auth model.** A single **mesh-wide bearer token** that every daemon accepts
  (simplest UX) plus the mandatory `Origin` allow-list; the serving daemon returns
  the host list via `host discover` so the client knows where to connect. The CLI
  path stays mesh-trust (no token). **Trade-off accepted:** a leaked token
  compromises every host at once — remediation is rotating the mesh token. (A
  per-host token model can be adopted later without protocol changes if the blast
  radius becomes a concern.)
- **Mesh CA.** `pohunek` issues a root CA; each daemon gets a **short-lived leaf
  cert auto-renewed** by `pohunek`, with a SAN covering **both** the NetBird IP and
  the magic-DNS name. The root key lives on **one designated host** (offline-
  capable) and rotates rarely; the root is installed once per device.
- **API stability.** **No compatibility promise yet:** the protocol is documented
  and version-negotiated, but stability is not committed until a second independent
  client exists. Generated TS types track the Rust source via the CI drift check;
  breaking changes are allowed (with a version bump) until the promise is made.
- **GUI stack.** A **lightweight SPA** with xterm.js in a `web/` workspace,
  embedded into the binary via `include_dir!`. The exact view framework is chosen
  when Slice C is built.
- **Aggregator fallback.** Direct fan-out is the only model built. The
  single-endpoint aggregator is **recorded as a fallback only** — for environments
  where the mesh root CA cannot be installed on devices — and is not designed now.

## Exit Criteria

- The control protocol is a documented, versioned public API with generated,
  drift-checked TypeScript types.
- The opt-in daemon gateway serves the embedded GUI and bridges control + attach
  over authenticated, origin-checked WebSocket, bound only to NetBird.
- The browser control center drives the full multi-host session lifecycle and
  in-browser attach on desktop, and on mobile as a PWA via the mesh CA.
- Provider links are stored opaquely in the chassis, with provider integration
  fully in the control center.
- No native GUI and no mandatory central server were introduced.
