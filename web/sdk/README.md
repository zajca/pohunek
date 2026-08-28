# @pohunek/sdk

TypeScript runtime client for the pohunek control protocol. The package is
private to this repository, mirrors the Rust SDK surface, and uses generated
types from `@pohunek/protocol`.

## Install and Build

From the repository root:

```bash
cd web
bun install --frozen-lockfile
bun run typecheck
bun run lint
bun test
```

There is no npm publishing contract for this package and no stability promise
before pohunek 1.0. Consume it from the Bun workspace or a local checkout.

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

## Subscribe

`subscribe` consumes the control connection after the acknowledgement, so use a
separate client when you also need ordinary requests:

```ts
import { SUPPORTED_PROTOCOL_VERSIONS, connectLocal, type Request } from "@pohunek/sdk";

const socketPath = process.env["POHUNEK_SOCKET"];
if (socketPath === undefined) {
  throw new Error("set POHUNEK_SOCKET to the pohunek daemon socket path");
}

const request: Request = {
  v: SUPPORTED_PROTOCOL_VERSIONS,
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
`session.output`, and `session.wait`, including their dedicated connections. A
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
opening the dedicated connection. Deduplicate evidence by `(activity_epoch, runtime,
activity_revision)`, since daemon reconnect preserves the runtime but starts a
new epoch-scoped revision sequence. Missing or contradictory evidence fails closed as
`session_input_wait_contract_mismatch`; it is never treated as delivery
confirmation from a daemon that ignored `wait`. Its recovery hint treats delivery
as unknown and forbids blind retry; inspect the session before deciding whether
to resend.

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
attach traffic to `/daemon/<host>/attach` binary frames. The relay core lives in
`web/backend` as `@pohunek/relay`.

## Regenerating Protocol Types

Generated TypeScript protocol files live under `web/shared/src/generated/**` and
fixtures live under `web/shared/fixtures/**`. Do not edit them by hand.

From the repository root:

```bash
cargo xtask ts generate
cargo xtask ts check
```

`cargo xtask ts generate` refreshes the TypeScript output from the Rust protocol
source. `cargo xtask ts check` is the CI drift gate and fails when the generated
files differ from what the Rust source would produce.
