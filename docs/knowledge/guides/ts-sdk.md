---
type: Guide
id: guide/ts-sdk
title: TypeScript SDK
description: Use the TypeScript package surfaces, the WebSocket relay transport contract, and the testkit helpers that drive Pohunek from Bun, Node, and browsers.
source_kind: manual
intents: [setup, debug, help]
---

# TypeScript SDK

The TypeScript workspace under `sdk/ts/` is a client of the public protocol. It
adds no second protocol and owns no daemon state: each daemon still owns its
logical sessions, events, and notifications, while per-session workers own live
PTYs. User interfaces built on it, such as the web control center, live outside
this repository in the external `zajca/pohunek-work` client repository.

## Package surfaces

- `@pohunek/protocol`: types generated from the Rust protocol source
  (`cargo xtask ts generate`; `cargo xtask ts check` is the drift gate).
- `@pohunek/protocol/relay`: the Rust team relay's wire types.
- `@pohunek/sdk`: the shared runtime plus Bun/Node Unix and TCP transports.
- `@pohunek/sdk/browser`: the browser-safe entry with only the WebSocket path;
  it contains no `node:net` dependency.
- `@pohunek/testkit`: the stateful fixture daemon, the real-daemon process
  helpers, and the loopback test relay used by tests.

The three packages ship as release tarballs (`pohunek-ts-protocol`,
`pohunek-ts-sdk`, `pohunek-ts-testkit`); `sdk/ts/sdk/README.md` has the install
and usage details.

## WebSocket relay transport

Browser code cannot dial daemon sockets or overlay TCP, so it imports `Client`
from `@pohunek/sdk/browser` and calls `Client.connectWs(origin, host)` against
an HTTP origin that relays to the daemon. The relay only tunnels the public
newline-delimited JSON control frames and raw attach bytes:

- `/daemon/<host>/control` carries one newline-free text frame per control
  line, at most `MAX_CONTROL_LINE_BYTES` long.
- `/daemon/<host>/attach` carries the raw attach bytes as binary frames.

A relay closes the tunnel on a frame of the wrong type, an oversized control
line, an unreachable daemon, or a daemon write queue that outgrows its bound.
Production relays, and their bind and trust policy, belong to the client that
hosts them. `@pohunek/sdk` implements only the client side of this contract
(`WsTransport`).

## Test relay

`startTestRelay` from `@pohunek/testkit/bun-relay` is a real relay over real sockets for
transport tests: it implements the routes and framing above and tunnels each
WebSocket to a Unix or TCP daemon target. It binds only to loopback and refuses
any other bind address with an error. It is a testing helper, not a deployable
relay. The `@pohunek/testkit` root entry runs on Node >= 20 and Bun; the
`bun-relay` subpath is Bun-only because it is built on `Bun.serve`.

## Runtime paths

`@pohunek/sdk` resolves the daemon socket with the same contract as the Rust
host components (`crates/paths/fixtures/runtime-paths.json` drives both
implementations):

- An explicit, valid absolute `XDG_RUNTIME_DIR` selects
  `$XDG_RUNTIME_DIR/pohunek/daemon.sock` on Linux and macOS. An empty,
  relative, parent-component, or NUL-containing value is a configuration error,
  never treated as absent.
- Without it, Linux fails fast and macOS uses
  `/private/tmp/pohunek-<effective-uid>/daemon.sock`; `TMPDIR` plays no role.
- A socket path over the platform limit (103 bytes on macOS, 107 on Linux)
  fails with the offending value instead of failing inside the connect call.
- Derived socket paths are verified before use: the runtime directory must be a
  real directory without symlinked components, owned by the current user with
  mode exactly `0700`, and a present socket must be a socket of the same user.
  The macOS default lives under the shared `/private/tmp`, where another local
  user could otherwise pre-create the predictable path.

## Verification

From the repository root, the SDK workspace gates are `bun install
--frozen-lockfile`, `bun run typecheck`, `bun run lint`, `bun test`, and `bun
test sdk/ts/scripts` (the release pack contract). The real-daemon suites
(`sdk/ts/sdk/test/e2e.test.ts` and `hermes-plugin.e2e.test.ts`) run with
`POHUNEK_E2E=1` against the built `pohunekd`, `pohunek-sessiond`, and
`pohunek` binaries.
