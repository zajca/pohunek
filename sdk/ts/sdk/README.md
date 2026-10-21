# @pohunek/sdk

TypeScript runtime client for the pohunek control protocol. The package is
private to this repository, mirrors the Rust SDK surface, and uses generated
types from `@pohunek/protocol`.

## Install and Build

From the repository root:

```bash
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun test
```

There is no npm registry publishing for this package and no stability promise
before pohunek 1.0. Consume it from the Bun workspace, a local checkout, or the
`pohunek-ts-sdk-X.Y.Z.tgz` release asset pinned by URL in `package.json`; its
`@pohunek/protocol` dependency is rewritten to the sibling release asset of the
same tag (see "TypeScript SDK Surface" in `docs/public-api.md`).

The release asset contains compiled ES modules and `.d.ts` declarations (not
TypeScript sources), so it runs under Bun and Node >= 20 (direct socket
transport) and Node >= 22 (WebSocket transport), and type-checks under
`moduleResolution` `bundler` and `nodenext`. The browser entry is
`@pohunek/sdk/browser`. The Bun workspace inside this repository resolves the
TypeScript sources directly.

## Connect and Call

Use the direct socket transport from Bun or Node:

```ts
import { connectLocal } from "@pohunek/sdk";

const socketPath = process.env["POHUNEK_SOCKET"];
if (socketPath === undefined) {
  throw new Error("set POHUNEK_SOCKET to the pohunek daemon socket path");
}

const client = await connectLocal(socketPath);

try {
  const protocolVersion = await client.handshake();
  const sessions = await client.call("session.list", {});

  console.log({ protocolVersion, sessionCount: sessions.length });
} finally {
  await client.close();
}
```

For a direct NetBird TCP daemon address, preserve the logical host name in the
first argument and pass the dial address separately:

```ts
import { connectTcp } from "@pohunek/sdk";

const client = await connectTcp("build-box", {
  host: "100.64.10.20",
  port: 7878,
});
```

## Lifecycle Budgets

Typed `session.new`, `session.resume`, `session.fork`, `session.stop`,
`session.remove` and `session.remove_accepting_unconfirmed` calls run on a
dedicated connection whose response budget covers the daemon's bounded work
(`SESSION_*_REQUEST_BUDGET_MS`, derived from `LIFECYCLE_STEP_BOUND_MS`,
`RESUME_SWEEP_PASSES` and `WORKTREE_HOOK_TIMEOUT_MS`, and equal to the Rust
client's). An explicit `requestTimeoutMs` replaces the budget. An elapsed
deadline of resume, fork, stop or remove rejects with `ClientError.kind`
`sessionResumeOutcomeUnknown`, `sessionForkOutcomeUnknown`,
`sessionStopOutcomeUnknown` or `sessionRemoveOutcomeUnknown`: the daemon may
still complete the call, and a retried fork creates a second child. Code that
switches exhaustively on `ClientError.kind` must handle these kinds.

## Subscribe

`subscribe` consumes the control connection after the acknowledgement, so use a
separate client when you also need ordinary requests:

```ts
import { CLIENT_PROTOCOL_VERSIONS, connectLocal, type Request } from "@pohunek/sdk";

const socketPath = process.env["POHUNEK_SOCKET"];
if (socketPath === undefined) {
  throw new Error("set POHUNEK_SOCKET to the pohunek daemon socket path");
}

const request: Request = {
  v: CLIENT_PROTOCOL_VERSIONS,
  id: "readme-subscribe-1",
  method: "subscribe",
  params: null,
};

const eventClient = await connectLocal(socketPath);

try {
  const subscription = await eventClient.subscribe(request);
  const event = await subscription.nextEvent();

  if (event?.event === "agent_state") {
    console.log(event.session_id, event.activity, event.source);
  }
} finally {
  await eventClient.close();
}
```

`nextEvent()` returns the generated `ProtocolEvent` union for known events,
`CatchAllEvent` for unknown event names, and `null` when the stream closes.
Use `nextLine()` when a caller needs raw event JSON text.

## Request Origin

A TypeScript client hosted inside a managed Pohunek session supplies its origin
explicitly. The browser-safe SDK never reads `process.env`:

```ts
import { connectLocal } from "@pohunek/sdk";

const client = await connectLocal(socketPath, {
  origin: { sessionId: "s-origin", daemonId: "daemon-origin" },
});
```

The origin is an atomic pair of bounded safe identifiers. The SDK copies it to
ordinary calls, subscriptions, waiting `session.input`, waiting
`session.output`, `session.wait`, and `session.wait_turn`, including their
dedicated connections. A
partial, unsafe, oversized, or
conflicting pair is rejected before a control line is written. Omit `origin`
for ordinary browser or host-side clients that are not running inside a managed
session.

`client.sessionInput({ ..., wait })` opens a dedicated bounded connection and
accepts success only when the daemon returns the requested activity, its source,
the exact runtime identity, a nonempty daemon activity epoch, and a canonical
decimal activity revision. The wait timeout is the daemon's overall
delivery-and-activity deadline measured before delivery; the transport adds only
fixed response headroom. The SDK rejects a timeout outside `1..=8000` before
opening the dedicated connection and normalizes an omitted `wait.until` to `[]`
before validation and wire serialization. The target array is copied before the
first transport await, so caller mutation cannot change the in-flight contract.
Both `sessionInput(...)` and generic typed `call("session.input", ...)` route
waited input through this path. Delayed provider framing returns
`session_input_wait_unsupported` before delivery because the daemon cannot
safely revalidate approval state inside a worker-owned delay. Deduplicate evidence by `(activity_epoch, runtime,
activity_revision)`, since daemon reconnect preserves the runtime but starts a
new epoch-scoped revision sequence. Missing or contradictory evidence fails closed as
`session_input_wait_contract_mismatch`; it is never treated as delivery
confirmation from a daemon that ignored `wait`. Its recovery hint treats delivery
as unknown and forbids blind retry; inspect the session before deciding whether
to resend.

`session_input_timeout` uses the same no-blind-retry posture: delivery may have
completed, so inspect the current session before deciding whether to resend.

## Turn Waits

`client.sessionWaitTurn(params)` waits for the next `turn_completed`,
`agent_blocked`, `approval_required`, or `exited` event of up to
`MAX_SESSION_WAIT_TURN_TARGETS` sessions after an opaque cursor. Pass the
returned `cursor` as `after` to the next wait so no event is missed between
calls, and branch on `reason` (`event`, `timeout`, `cursor_reset`,
`targets_ended`); every result carries per-session `progress`.

```ts
let after: string | undefined;
for (;;) {
  const result = await client.sessionWaitTurn({
    session_ids: ["s-41", "s-42"],
    until: ["turn_completed", "agent_blocked", "approval_required", "exited"],
    ...(after === undefined ? {} : { after }),
    timeout_ms: 300_000,
  });
  after = result.cursor;
  if (result.reason === "event") {
    console.log(result.event?.session_id, result.event?.kind);
  } else if (result.reason === "cursor_reset") {
    // Events between the old cursor and the reset are lost, not absent:
    // inspect every target before waiting again with the fresh cursor.
    for (const progress of result.progress) {
      console.log(await client.call("session.inspect", progress.session_id));
    }
  } else if (result.reason === "targets_ended") {
    break;
  }
}
```

The call runs on a dedicated connection whose response deadline is
`timeout_ms` plus fixed headroom, or `MAX_SESSION_WAIT_TURN_MS` plus headroom
when `timeout_ms` is omitted and the daemon default applies, so the client's
`requestTimeoutMs` never cuts a valid wait short. Generic typed
`call("session.wait_turn", ...)` takes the same path. A `timeout_ms` that is not
a nonzero `u32` fails with `daemon/bad_request` before anything is sent. A
daemon of the previous protocol version makes the call fail with
`daemon_protocol_too_old` before it is sent; `host.inspect` reports
`session_wait_turn_supported` on daemons that serve it. See
[Turn Waits](../../../docs/public-api.md#turn-waits) for the full contract.

## Attach

First request an attach stream over the control connection, then redeem it on a
raw socket stream:

```ts
import { attachRawLocal, connectLocal } from "@pohunek/sdk";

const socketPath = process.env["POHUNEK_SOCKET"];
if (socketPath === undefined) {
  throw new Error("set POHUNEK_SOCKET to the pohunek daemon socket path");
}

const client = await connectLocal(socketPath);
const session = await client.call("session.new", {
  agent: "shell",
  cols: 80,
  rows: 24,
});
const attached = await client.call("session.attach", { session_id: session.id });
const raw = await attachRawLocal(socketPath, attached.stream_id);

try {
  const writer = raw.writable.getWriter();
  await writer.write(new TextEncoder().encode("pwd\r"));
  writer.releaseLock();
} finally {
  await raw.close();
  await client.call("session.detach", { stream_id: attached.stream_id }).catch(() => undefined);
  await client.call("session.stop", session.id).catch(() => undefined);
  await client.close();
}
```

`connectRawLocal`, `connectRawTcp`, and `connectRawWs` open raw byte channels
without writing the attach prelude. `attachRawLocal`, `attachRawTcp`, and
`attachRawWs` write exactly one prelude and return a `RawStream`.

## Remote Hosts

`resolveHost` maps a host route, as `pohunek --host` accepts it, to this machine
or to one overlay peer by asking the local daemon. `connectHost` and `attachRaw`
use it to reach a remote host:

```ts
import { connectHost, resolveHost } from "@pohunek/sdk";

const socketPath = process.env["POHUNEK_SOCKET"];
if (socketPath === undefined) {
  throw new Error("set POHUNEK_SOCKET to the pohunek daemon socket path");
}

const resolved = await resolveHost("build-box", socketPath);
if (resolved.target === "remote") {
  // Later lookups of `pinnedRoute` keep selecting this peer even if the name
  // `build-box` is reassigned.
  console.log(resolved.address, resolved.pinnedRoute);
}

const client = await connectHost("build-box", socketPath);
try {
  console.log(await client.call("session.list", null));
} finally {
  await client.close();
}
```

The daemon applies the CLI's selector rules, including this machine's own
overlay name (`local`) and the typed `host_unknown` and `overlay_host_ambiguous`
errors. `host.resolve` exists from protocol 4 and is served on the local socket
only; against a daemon of the previous release the calls fail with
`daemon_protocol_too_old`.

## WebSocket Relay

Browsers use the relay transport because they cannot dial daemon sockets or
NetBird TCP directly:

```ts
import { Client, attachRawWs } from "@pohunek/sdk";

const relayUrl = "https://relay.example.internal";
const host = "build-box";
const sessionId = process.env["POHUNEK_SESSION_ID"];
if (sessionId === undefined) {
  throw new Error("set POHUNEK_SESSION_ID to a live pohunek session id");
}

const client = await Client.connectWs(relayUrl, host);
const attached = await client.call("session.attach", { session_id: sessionId });
const raw = await attachRawWs(relayUrl, host, attached.stream_id);
```

The relay maps control traffic to `/daemon/<host>/control` text frames and raw
attach traffic to `/daemon/<host>/attach` binary frames. The Bun-only
`@pohunek/testkit/bun-relay` subpath exports `startTestRelay`, a loopback-only
implementation of this relay for tests; production relays belong to the client
that hosts them. The `@pohunek/testkit` root entry runs on Node >= 20 and Bun.

## Regenerating Protocol Types

Generated TypeScript protocol files live under `sdk/ts/protocol/src/generated/**` and
fixtures live under `sdk/ts/protocol/fixtures/**`. Do not edit them by hand.

From the repository root:

```bash
cargo xtask ts generate
cargo xtask ts check
```

`cargo xtask ts generate` refreshes the TypeScript output from the Rust protocol
source. `cargo xtask ts check` is the CI drift gate and fails when the generated
files differ from what the Rust source would produce.
