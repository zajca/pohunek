# TypeScript SDK plan (2026-07-05) — Track S.3, `web/shared` + `web/sdk`

> **Status and supersession (2026-09-03):** This completed plan produced the
> current protocol-v3 TypeScript SDK and owner-only, mesh-local transparent Bun
> WebSocket transport. References below to `@pohunek/relay` describe that
> historical package role, not the accepted public team relay. The future Rust
> `pohunek-relayd` and its client API follow the
> [team-relay RFC](team-relay-control-plane-rfc.md),
> [#71](https://github.com/zajca/pohunek/issues/71), and
> [#86](https://github.com/zajca/pohunek/issues/86). The Bun backend remains the
> supported owner-path WebUI; #86 adds a separate typed team-relay client.

Implements **Track S.3** from [`ROADMAP.md`](../ROADMAP.md) and Slice A items
3–4 of [`phases/04-browser-control-center.md`](../phases/04-browser-control-center.md):
a TypeScript SDK for the pohunek control protocol with `ts-rs`-generated types,
a runtime client with pluggable transports, a first-class attach duplex stream,
and a CI drift check that fails when generated TS types diverge from the Rust
protocol source.

Prerequisites already complete:

- **S.1** — Rust SDK extracted to `crates/client` (transport, subscription,
  raw attach stream, `ClientError`).
- **S.2** — versioned public API documented in
  [`public-api.md`](../public-api.md) (envelopes, methods, error contract,
  events, attach stream).

The TS SDK mirrors the Rust SDK surface (`Client`, `ClientOptions`,
`Subscription`, `RawStream`, `ClientError`, typed `call<M>`), so
`docs/public-api.md` stays the single contract for both.

## Decisions

### Locked earlier (Phase 4 / ROADMAP) — reused as-is

- **Layout:** Bun workspaces under `web/` — `web/shared` (generated types),
  `web/sdk` (runtime client). `web/backend` / `web/frontend` are Track B, not
  built here.
- **Type generation:** `ts-rs`, TS types derived directly from the Rust
  protocol structs; no separate runtime validator — protocol version
  negotiation governs compatibility.
- **Transports:** pluggable — TCP/Unix (Node/Bun → daemon direct) now;
  WebSocket (browser → aggregator backend) as a second transport behind the
  same interface.
- **Attach:** the raw byte stream is a first-class SDK primitive (duplex
  stream), not an afterthought.
- **Stability/publishing:** structured as publishable packages, but npm
  publishing and any stability promise stay deferred pre-1.0. SDK version
  tracks the protocol version; breaking changes allowed with a version bump.
- **TS hygiene:** `strict: true`, ESLint, explicit return types on public
  functions.

### Made by this plan

1. **ts-rs v12, behind a cargo feature.** `crates/protocol` gains an optional
   `ts` feature (`ts-rs` v12 with `serde-compat` default + `serde-json-impl`
   for the few `serde_json::Value` fields). Normal daemon/CLI/GUI builds do
   not compile ts-rs. No chrono impl needed — protocol timestamps are already
   RFC3339 `String`s.
2. **Generation is an xtask, not a bare cargo test.** `cargo xtask ts generate`
   wraps `cargo test -p pohunek-protocol --features ts export_bindings` with
   `TS_RS_EXPORT_DIR` pointed at `web/shared/src/generated/`, then emits the
   non-derivable files (barrel index, method map, version/event constants).
   `cargo xtask ts check` regenerates into a temp dir and diffs against the
   committed output — that diff is the CI drift gate.
3. **Envelopes are hand-written in the SDK; domain types are generated.**
   `Request`/`Response`/`Event` are tiny, stable, and shaped in ways ts-rs
   represents poorly (`#[serde(untagged)]` two-variant response,
   `#[serde(flatten)] payload: Value`). The SDK owns envelope framing anyway
   (exactly like the Rust SDK owns it in `crates/client/src/transport.rs`).
   Everything method- and event-facing (params, results, `SessionInfo`,
   `NotificationRecord`, `ProtocolError`, enums, …) is generated — never
   hand-maintained.
4. **The method map is generated from the Rust `Method` trait.** A generator
   (feature-gated in `crates/protocol`, invoked by `cargo xtask ts generate`)
   walks the same marker types `crates/protocol/src/method.rs` defines and
   emits `web/shared/src/generated/methods.ts`: a
   `interface Methods { "session.list": { params: …; output: … } }` map keyed
   by wire name. This is the TS twin of `Client::call::<M>` — the pairing of
   name/params/output cannot drift because it is derived from the same macro
   table.
5. **Typed event payloads move into `crates/protocol` (additive).** Today only
   notification events have typed payload structs; session/agent/attach event
   payloads are ad-hoc `json!` at the daemon emit sites. This plan adds
   `SessionEvent`-family payload structs (`{session: SessionInfo}`,
   `{session_id, activity, source}`, `{session_id, stream_id}`) to
   `crates/protocol`, makes the daemon construct them (serialization output
   byte-identical), and generates the TS discriminated union from them. This
   is the only daemon-touching work in the plan and it is additive.
6. **Transports are built on `node:net`,** which Bun implements natively —
   one code path for Bun and Node, Unix socket + TCP for free, no
   `Bun.connect`-only API. The Phase 4 risk note (Bun byte-stream
   backpressure) is covered by an attach throughput test, with Node as the
   documented fallback runtime.
7. **Attach duplex = Web Streams** (`ReadableStream<Uint8Array>` +
   `WritableStream<Uint8Array>`). Works in Bun, Node ≥ 18, and browsers, so
   the same `RawStream` shape survives the future WebSocket transport.
8. **WebSocket transport is implemented now, with a production relay as its
   peer.** The daemon never grows a WS surface, so the WS transport's peer is
   the aggregator backend — and instead of a throwaway test bridge, this plan
   builds the backend's **transport core** as a real module: `web/backend`
   starts life as a pure 1:1 relay (`pohunek-relay`). Framing per Phase 4's
   browser-facing protocol decision: one WS per control connection carrying
   the newline-delimited JSON control lines verbatim as text frames; one WS
   per attach carrying raw bytes as binary frames. Host routing lives in the
   URL (`/daemon/<host>/control`, `/daemon/<host>/attach`), so each WS
   connection is a transparent tunnel to exactly one daemon and the relay
   holds no protocol logic and no state. Aggregation, SPA serving, providers,
   TLS, and auth remain Track B application work **on top of** this module.
   The browser-facing URL/auth surface is explicitly re-reviewable when
   Track B starts — pohunek is pre-1.0 with no back-compat promise, so
   freezing it now is low-risk and buys a fully tested transport instead of a
   spec on paper.
9. **NetBird resolution stays out of the SDK core.** Core connect targets are
   an explicit Unix socket path or `host:port`. Callers get addresses from
   `host.discover` (via a local daemon) or their own config. A
   `netbird`-CLI-shellout resolver helper can be added later as a separate
   module without touching the core (mirrors how `crates/client` isolates
   NetBird in its `connect(host, …)` path).
10. **Package names:** `@pohunek/protocol` (`web/shared`) and `@pohunek/sdk`
    (`web/sdk`), both `"private": true` until publishing is decided.
11. **Generated output is committed.** `web/shared/src/generated/` is checked
    in (marked `linguist-generated` in `.gitattributes`) so `web/` consumers
    build without a Rust toolchain and the drift check is a plain diff.
12. **Cross-language golden fixtures prove the hand-written layer.** The
    hand-written TS envelope layer (decision 3) must not become the one place
    Rust and TS can disagree. `cargo xtask ts generate` also emits canonical
    JSON fixtures serialized by the Rust protocol types — request/ok/err/event
    envelopes plus representative payloads (`SessionInfo` with and without
    optional fields, `ProtocolError` with and without `recover`, a tagged
    filter, a `NotificationRecord`) — into `web/shared/fixtures/`. Bun tests
    parse every fixture through the TS guards and typed decoders; the drift
    check covers fixtures exactly like generated types. Optional-field
    semantics (`skip_serializing_if` omission vs `null`) are thereby asserted
    against real Rust serde output, not assumptions.

## Non-goals (this track)

- No Track B application layer: no aggregation/fan-out, no SPA, no PWA, no
  provider seam, no TLS, no auth. `web/backend` here is only the pure relay
  module (decision 8) that Track B later builds on.
- No npm publishing, no semver stability promise.
- No runtime schema validation of payloads.
- No NetBird resolution helper in the SDK core.
- No daemon behavior or wire-format changes — decision 5 is a pure
  refactor/typing change with byte-identical serialization.

## Deliverable layout

```
web/
  package.json            # bun workspace root: shared, sdk
  tsconfig.base.json      # strict, ES2022+, bundler resolution
  eslint.config.js
  shared/                 # @pohunek/protocol
    package.json
    fixtures/             # golden JSON serialized by Rust (decision 12)
    src/
      generated/          # ts-rs output + generated methods.ts, constants.ts
      index.ts            # barrel (hand-written, re-exports generated)
  sdk/                    # @pohunek/sdk
    package.json
    src/
      envelope.ts         # hand-written Request/Response/Event + guards
      error.ts            # ClientError taxonomy (mirrors crates/client/src/error.rs)
      framing.ts          # newline-delimited JSON codec, 1 MiB line cap
      transport.ts        # Transport interface + connect options
      transport-socket.ts # node:net Unix + TCP transport
      transport-ws.ts     # WebSocket transport (browser + Bun/Node)
      client.ts           # Client: call<M>, request, handshake, subscribe
      subscription.ts     # Subscription: nextLine/nextEvent + typed event union
      attach.ts           # RawStream, connectRaw*, attachRaw* (prelude write)
      index.ts
    test/
      mock-daemon.ts      # in-process node:net scripted daemon
      *.test.ts
      e2e.test.ts         # against a real pohunekd (env-gated)
  backend/                # @pohunek/relay — Track B transport core (decision 8)
    package.json
    src/
      relay.ts            # WS <-> daemon TCP/Unix 1:1 tunnel (control + attach)
      main.ts             # bind to NetBird addr (fail-closed, never 0.0.0.0)
    test/
```

## Phases

Rust edits in Phases 0–2 are subject to the repo rule: read the relevant
`.agents/rust-guidelines/` files first; clippy `-D warnings` and
`cargo xtask docs check` gate every step.

### Phase 0 — ts-rs coverage spike (throwaway branch, keep findings)

Goal: prove ts-rs v12 renders every exported `crates/protocol` type
acceptably **before** committing to per-type annotations.

- [ ] Add the `ts` feature + `ts-rs` v12 (`serde-json-impl` feature) to
      `crates/protocol/Cargo.toml`; derive `TS` + `#[ts(export)]` on all
      exported wire types (everything in `crates/protocol/src/lib.rs` re-exports
      except `Request`/`Response`/`Event` per decision 3).
- [ ] Run the export against a scratch dir; review generated output for the
      known-risky shapes: `SessionId`/`NotificationId` newtypes (expect
      `type SessionId = string`), internally/externally tagged filter enums
      (`SessionListFilter`, `NotificationListParams` filters), `HashMap`
      metadata maps, `Option` fields with `skip_serializing_if` (decide
      `#[ts(optional)]` policy: field absent vs `| null` — must match what the
      daemon actually emits), `serde_json::Value` fields, and
      `ProtocolError`/`ErrorClass`.
- [ ] Record per-type overrides needed (`#[ts(as = …)]`, `#[ts(type = …)]`,
      `#[ts(optional)]`) and any unsupported-serde-attr warnings in the PR
      description; fail the spike if any type would require hand-editing
      generated output.
- *Check:* generated `.ts` for `SessionInfo`, `NotificationRecord`,
  `ProtocolError`, and one params/result pair per method family compiles under
  `tsc --strict` and matches the JSON the daemon emits today (spot-verify
  against `docs/public-api.md` examples).

### Phase 1 — Rust side: derives, typed event payloads, generators, xtask

**Files:** `crates/protocol/Cargo.toml`, all `crates/protocol/src/*.rs`,
`crates/daemon/src/**` (event emit sites only), `crates/xtask/src/lib.rs`,
`crates/xtask/src/ts.rs` (new), `Cargo.toml` (workspace).

- [ ] Land the Phase 0 derives + overrides behind the `ts` feature, with
      `cargo hack --feature-powerset` staying green (CI already runs it).
- [ ] Add typed event payload structs (decision 5) to `crates/protocol`:
      session lifecycle (`{session}`), `agent_state`
      (`{session_id, activity, source}`), `attach_opened`/`attach_closed`
      (`{session_id, stream_id}`). Unit-test that their serialization is
      byte-identical to the current daemon `json!` payloads, then switch the
      daemon emit sites to construct them.
- [ ] Write the method-map generator: a `ts`-feature-gated function in
      `crates/protocol` (or the xtask) that iterates the `method_marker!`
      table and emits `methods.ts` (wire name → params/output TS type names,
      importing from the generated barrel), plus `constants.ts`
      (`PROTOCOL_VERSION`, event name constants from `protocol::event`,
      `MAX_CONTROL_LINE_BYTES`, attach prelude shape).
- [ ] Add `cargo xtask ts generate` (export + generators + barrel into
      `web/shared/src/generated/`, deterministic output: stable ordering,
      fixed header comment) and `cargo xtask ts check` (regenerate to temp
      dir, diff, non-zero exit on drift, actionable message naming the
      command to run).
- [ ] Emit the golden fixtures (decision 12) from the same xtask run: each
      fixture is serialized by the real Rust protocol types (not hand-typed
      JSON), covering envelopes and the representative payload set, written
      deterministically to `web/shared/fixtures/` and included in the drift
      diff.
- [ ] Update `docs/knowledge/` + `docs/public-api.md` ripple for the new
      typed event structs if `cargo xtask docs check` flags any surface.
- *Check:* `cargo xtask ts generate` is idempotent (second run = no diff);
  `cargo xtask ts check` fails after a deliberate protocol struct edit and
  passes after regeneration; full Rust gate set green.

### Phase 2 — `web/` workspace scaffolding

**Files:** `web/package.json`, `web/tsconfig.base.json`,
`web/eslint.config.js`, `web/shared/**`, `web/sdk/**` (skeleton),
`.gitignore`, `.gitattributes`, `AGENTS.md`.

- [ ] Bun workspace root: pinned Bun version (`packageManager` field +
      `.bun-version`), workspaces `["shared", "sdk", "backend"]`, scripts for
      `typecheck` / `lint` / `test` fanning out to packages.
- [ ] `@pohunek/protocol` (`web/shared`): commits the Phase 1 generated
      output; hand-written `src/index.ts` barrel only. No runtime code.
- [ ] `@pohunek/sdk` (`web/sdk`): package skeleton depending on
      `@pohunek/protocol` via `workspace:*`.
- [ ] `tsconfig.base.json` with `strict: true`, `noUncheckedIndexedAccess`,
      `exactOptionalPropertyTypes` (validate against the Phase 0 optional-field
      policy), explicit-return-type lint rule in ESLint.
- [ ] Repo hygiene: ignore `node_modules`/`dist`; mark
      `web/shared/src/generated/**` `linguist-generated=true`; extend
      `AGENTS.md` build/test/lint gates with the `web/` commands
      (`bun install --frozen-lockfile`, `bun run typecheck`, `bun run lint`,
      `bun test`) and the `cargo xtask ts check` rule ("protocol change is not
      done until bindings regenerate").
- *Check:* `bun install && bun run typecheck && bun run lint` green on the
  scaffold; committed generated types compile with zero manual edits.

### Phase 3 — SDK core: envelopes, framing, errors, typed client

**Files:** `web/sdk/src/envelope.ts`, `framing.ts`, `error.ts`,
`transport.ts`, `client.ts`, `subscription.ts`, tests + `test/mock-daemon.ts`.

Mirror `crates/client` semantics exactly; its tests
(`crates/client/tests/request_response.rs`, `subscription.rs`) are the spec —
port their scenarios.

- [ ] `envelope.ts`: hand-written `Request`/`Response`/`Event` types + narrow
      runtime guards (`ok` xor `err`; `v`/`id` presence). Response parse
      mirrors the Rust untagged order: try `ok`, then `err`.
- [ ] `framing.ts`: newline-delimited JSON encoder/decoder over a byte
      stream; enforce the 1 MiB control-line cap on both directions with the
      same error class the Rust SDK uses (`transport/framing`).
- [ ] `error.ts`: `ClientError` mirroring `crates/client/src/error.rs`
      variants (daemon unreachable, framing, daemon protocol error
      passthrough, host unreachable, remote daemon unavailable, io, json) +
      `toProtocolError()` producing the same `{class, code, msg, recover}`
      envelope taxonomy. Clients branch on `class`/`code`, never `msg`.
- [ ] `transport.ts`: `Transport` interface = a connected duplex byte channel
      factory with two capabilities — `control()` (framed lines) and `raw()`
      (unframed, for attach) — plus `ConnectOptions`
      (`connectTimeoutMs`/`requestTimeoutMs`, defaults 5000 to match
      `ClientOptions` in Rust). Both the socket and WS transports implement
      this interface; nothing above it may know which one is in play.
- [ ] `client.ts`: `Client.connectLocal(socketPath)`,
      `Client.connectTcp(host, addr)` (host kept for error context, as in
      Rust), `call<M extends keyof Methods>(method, params)` typed off the
      generated `Methods` map, `request(Request)` raw escape hatch,
      `handshake()` (`daemon.health` → protocol version, strict-equality
      check against generated `PROTOCOL_VERSION`), request-id generator
      matching `next_request_id(method)` format, per-request timeout.
- [ ] `subscription.ts`: `subscribe()` consumes the connection after the
      `{subscribed: true}` ack; `nextLine()` (raw) and `nextEvent()` decoding
      into the generated typed event union (discriminated on `event`);
      unknown event names surface as a catch-all variant, not an error
      (additive evolution rule).
- [ ] `test/mock-daemon.ts`: in-process `node:net` server (Unix + TCP)
      driven by scripted exchanges — the TS twin of the Rust SDK's test
      listeners.
- [ ] Golden-fixture tests (decision 12): every file in
      `web/shared/fixtures/` must parse through the envelope guards and typed
      decoders; a fixture the TS layer cannot represent is a failing test,
      not a skip.
- *Check (bun test, no real daemon):* ok/err/garbled/closed/oversized-line
  scenarios produce the same error classes/codes as the Rust SDK tests
  assert; `call` on a version-mismatched response surfaces
  `daemon/version_mismatch`; subscription yields typed events and tolerates
  an unknown event name; all golden fixtures decode.

### Phase 4 — attach raw stream

**Files:** `web/sdk/src/attach.ts`, tests.

- [ ] `connectRaw*` (Unix/TCP) opening an unframed connection;
      `attachRaw(target, streamId)` writes exactly one prelude line
      (`{"attach":"a-1"}\n`) and returns a `RawStream` (Web Streams duplex
      pair + `close()`).
- [ ] Failed redemption path: before switching to raw mode, a normal error
      response on the second connection must parse and reject with the typed
      `ClientError` (daemon replies per `docs/public-api.md` "Attach Stream"
      rule).
- [ ] Backpressure test (the Phase 4 Bun risk): round-trip a multi-MB byte
      stream through the mock daemon with a slow reader; assert no loss, no
      unbounded buffering (writer respects `desiredSize`).
- *Check:* prelude bytes exact; binary payloads (non-UTF-8) round-trip
  unmodified; `close()` ends the stream without affecting the control
  connection.

### Phase 5 — WebSocket transport + relay transport core

**Files:** `web/sdk/src/transport-ws.ts`, `web/backend/**`, tests.

The WS transport and the relay are two halves of one wire contract
(decision 8); they land together and are tested against each other.

- [ ] `web/backend` (`@pohunek/relay`): a pure 1:1 tunnel process.
      `/daemon/<host>/control` upgrades to a WS whose text frames are control
      lines relayed verbatim to one daemon connection (dialed exactly like
      the socket transport dials it); `/daemon/<host>/attach` relays binary
      frames to a raw daemon connection. One WS = one daemon connection; WS
      close tears down the daemon connection and vice versa. No parsing of
      relayed lines beyond the 1 MiB cap, no state, no aggregation.
- [ ] Relay bind semantics reuse the daemon's fail-closed rule
      (`validate_netbird_bind_addr` twin): bind only to the host's NetBird
      address, loopback when NetBird is absent, never `0.0.0.0`. Host
      resolution for the `<host>` segment is the relay operator's config
      (static map or local-daemon `host.discover`), not relay magic.
- [ ] `transport-ws.ts`: implements the Phase 3 `Transport` interface over
      WebSocket — text frames for `control()`, a second WS with binary
      frames for `raw()`. Uses the WHATWG `WebSocket` API only (browser,
      Bun, Node ≥ 22), no ws-library dependency; backpressure honored via
      `bufferedAmount` polling on send and stream `desiredSize` on receive.
- [ ] The full Phase 3 + Phase 4 test suites run a second time parameterized
      over the WS transport: mock daemon behind an in-process relay, same
      scenarios, same error taxonomy (relay-connection failures map to the
      same `transport`/`daemon_unreachable` classes the socket transport
      uses).
- *Check:* the SDK test matrix is transport-parameterized and green on both
  transports, including the attach backpressure test through the relay; a
  killed daemon connection closes the browser-side WS (and vice versa)
  without leaking sockets; the relay refuses a non-NetBird bind.

### Phase 6 — end-to-end against a real daemon + CI

**Files:** `web/sdk/test/e2e.test.ts`, `.github/workflows/ci.yml`,
`crates/xtask` (only if a helper is needed).

This is the ROADMAP *done when* for S.3, TS edition: health, list, event,
attach round-trip with no hand-written wire types.

- [ ] E2E harness: spawn a release/debug `pohunekd` with an isolated temp
      runtime/data dir (reuse the daemon's own test conventions from
      `crates/daemon/tests/health_socket.rs` for socket setup), gated behind
      `POHUNEK_E2E=1` so plain `bun test` stays hermetic.
- [ ] Scenario: `handshake()` → `session.new` (shell agent) → `session.list`
      shows it → `subscribe` observes `session_created`/`agent_state` →
      `session.attach` → `attachRaw` round-trips input/output bytes →
      `session.detach` + `session.stop` → daemon shutdown clean.
- [ ] Run the E2E scenario on both transports: socket transport directly
      against `pohunekd`, and WS transport through a spawned relay in front
      of the same `pohunekd`.
- [ ] CI: new `web` job — install Bun (pinned), `bun install
      --frozen-lockfile`, `bun run typecheck`, `bun run lint`, `bun test`;
      build `pohunekd` and run both E2E scenarios; add `cargo xtask ts check`
      to the existing docs/check job (drift gate).
- *Check:* CI red if (a) generated types or fixtures drift, (b) any TS gate
  fails, (c) either E2E scenario fails against the real daemon.

### Phase 7 — documentation

**Files:** `docs/public-api.md`, `docs/ROADMAP.md`, `docs/README.md`,
`docs/knowledge/**` (only if `docs check` flags), `web/sdk/README.md`.

- [ ] `docs/public-api.md`: add a "TypeScript SDK Surface" section mirroring
      the "Rust SDK Surface" section (exports, connect APIs, request APIs,
      attach helpers, error mapping, supported runtimes, both transports),
      and document the relay's WS framing contract (URL scheme, text/binary
      frame mapping, teardown semantics) with an explicit pre-1.0
      re-reviewable marker for Track B.
- [ ] `docs/ROADMAP.md`: mark S.3 complete; note that Track B inherits
      `web/backend` as its transport core instead of starting from zero.
- [ ] `web/sdk/README.md`: quickstart (connect, call, subscribe, attach), the
      no-stability-promise note, and the regeneration workflow
      (`cargo xtask ts generate`).
- [ ] Run `cargo xtask docs check`; update any flagged knowledge files.
- *Check:* `cargo xtask docs check` green; public-api.md documents every
  exported SDK symbol.

## Risks

- **ts-rs rendering gaps** (flatten/untagged/optional semantics) — mitigated
  by the Phase 0 spike before any commitment; envelopes are hand-written by
  design; worst case a type gets `#[ts(as = …)]` overrides, never hand-edited
  output.
- **Optional-field semantics drift** (`skip_serializing_if` vs `| null`):
  the daemon omits fields; TS must model them as optional properties, and
  `exactOptionalPropertyTypes` will surface any mismatch at compile time.
  Phase 0 fixes the policy once, globally.
- **Generated-output nondeterminism** breaking the drift check — the xtask
  owns ordering and headers; idempotence is an explicit Phase 1 check.
- **Bun/node:net backpressure on attach** — explicit throughput/backpressure
  test in Phase 4; Node ≥ 18 is a drop-in fallback runtime on the same API.
- **Toolchain surface in CI grows** (Bun) — pinned version, frozen lockfile,
  isolated `web` job so Rust jobs stay untouched.
- **Daemon event refactor regressions** (decision 5) — byte-identical
  serialization unit tests land *before* the emit sites switch.
- **The browser-facing WS surface may need changes when Track B's real
  requirements land** (auth, TLS, host discovery) — accepted deliberately:
  pohunek is pre-1.0 with no back-compat promise, the surface is confined to
  `transport-ws.ts` + `relay.ts`, and the alternative (an untested
  paper-spec transport) is the worse outcome.

## Done criteria (Track S.3)

- A minimal TS client built only on `@pohunek/sdk` performs `daemon.health`
  and `session.list`, subscribes and receives a typed event, and round-trips
  an attach byte stream — **on the socket transport directly against a real
  daemon, and on the WebSocket transport through the relay against the same
  daemon** — with no hand-written wire types outside the SDK's envelope
  layer, and that envelope layer proven against Rust-serialized golden
  fixtures.
- `cargo xtask ts check` gates CI: a protocol struct or fixture change
  without regenerated bindings fails the build.
- Track B starts from a working, tested transport core (`web/backend`
  relay + `transport-ws.ts`), not from a spec.
- All existing Rust gates stay green (`fmt`, `clippy -D warnings`, workspace
  tests, `cargo hack feature-powerset`, `docs check`), and the new `web`
  gates are documented in `AGENTS.md`.
