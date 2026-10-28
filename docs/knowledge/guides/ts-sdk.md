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

## Turn waits

`client.sessionWaitTurn(params)` calls `session.wait_turn` on a dedicated
connection whose deadline follows `timeout_ms` (or `MAX_SESSION_WAIT_TURN_MS`
when the daemon default applies), so the client's `requestTimeoutMs` never cuts
a long wait short; generic `call("session.wait_turn", ...)` takes the same
path. Against a protocol 3 daemon it fails with `daemon_protocol_too_old`
before sending. The `@pohunek/testkit` fixture daemon serves the method:
`scenario.recordTurnEvent(sessionId, kind, notificationId?)` records a turn
signal at once, standing for one the daemon already accepted and debounced,
`session.stop` records `exited` when the stopped state is committed, the
`turnWait` start option sets the default, ceiling, and journal size, and
`activeTurnWaiterCount` lets a test wait until a request is pending. The
fixture models no attention debounce or `turn_completed` drop rule, no hook
admission or notification-policy coupling, no waiter-pool limit, and no
hangup release.

## Package commands

`client.runCommand({ package, command, timeout_ms?, max_output_bytes? })`
(also `call("command.run", ...)`) runs a command an installed package declared
and returns the package identity, digest and verbatim stdout. Only those four
fields reach the wire. It uses a dedicated connection whose deadline is the wire
timeout (`MAX_COMMAND_TIMEOUT_MS` when omitted) plus response headroom, so the
client's `requestTimeoutMs` never cuts a run short and the budget of other calls
is unchanged; a `timeout_ms` or `max_output_bytes` that is not a nonzero `u32`
fails with `bad_request` before sending. A `command_*` error keeps its code;
`command_failed` exposes the exit status through `ClientError.details`
(`process_exit.exit_code` or `signal`). A daemon that answers `method_not_found`
or speaks protocol 3 fails with `command_unsupported`.

`package.install` and `package.link` send `accept_commands` only after
`host.inspect` reports `package_install_commands_supported` on the connection
that carries the request; otherwise the call fails with
`package_install_commands_unsupported` and nothing is sent, because an older
daemon rejects the key. `accept_commands: false` is left off the wire.

The testkit fixture daemon serves `command.run` for the commands its
`commands` start option lists (`package`, `command`, `output` or `error`, and a
`hold` promise that delays the reply). Without the option the method is unknown,
like an older daemon. It answers `command_package_not_installed` for a package
with no entry and `command_not_declared` for an unknown command of a listed
package. It runs no process and models no exposure, limits, busy slots or
shutdown.

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
`pohunek` binaries. The Hermes plugin e2e also needs the official Hermes
package archive (`POHUNEK_HERMES_PACKAGE`, from `scripts/build-hermes-package`)
and a throwaway catalog with its trust anchor (`POHUNEK_HERMES_CATALOG_DIR`,
from `cargo xtask hermes throwaway-catalog`), because the daemon serves
`hermes` only from that package.
