# SDKs and building your own client

pohunek ships no user interface. This page shows how to build one, or any other
tool, on the public protocol through the Rust and TypeScript SDKs. The
[TypeScript SDK guide](knowledge/guides/ts-sdk.md) covers package surfaces,
the WebSocket transport, and runtime paths in more detail.

pohunek's real interface is its **protocol**, not any one client. The CLI, the
web control center, and the native GUI in `zajca/pohunek-work` are consumers of
a versioned, newline-delimited JSON protocol that every client speaks — and the
same protocol and SDKs are available to you. You are encouraged to **build your
own GUI, TUI, launcher, or automation** on top of them rather than being tied
to a bundled app.

Nothing a bundled client does is private to it: hosts, sessions, projects,
worktrees, notifications, diffs, and `subscribe` event streams are all driven
entirely through this surface.

- **Rust** — the `pohunek-client` crate: a typed `Client`, transports for local
  Unix sockets and NetBird/WireGuard TCP, raw attach helpers, typed
  `ClientError`s, and `subscribe` streams. It re-exports the `protocol` crate,
  the source of truth for every request, response, and event type.
- **TypeScript** — `@pohunek/protocol` (types generated from the Rust protocol),
  `@pohunek/sdk` (Bun/Node plus shared runtime), its browser-safe
  `@pohunek/sdk/browser` entry, and `@pohunek/testkit` (the stateful fixture
  daemon and loopback test relay used by tests).
- **Contract** — the wire surface is documented in
  [`docs/public-api.md`](public-api.md); TS types are regenerated from
  Rust so the two SDKs never drift. The protocol is versioned, but pre-1.0 it
  may still change between releases (see the status note in the [README](../README.md)).
- **Distribution and pinning** — clients use public contracts only: the CLI
  with `--json` and the public protocol through the SDKs. Rust crates are
  consumed by git tag and the TypeScript packages as release tarballs pinned by
  URL and integrity, never from a registry. Crates beyond `pohunek-client` that a
  Rust client links are a pinned, not a stable, API (no back-compat shims), and
  clients pin the protocol version they were built against (a daemon one
  release newer still serves them through the protocol window).
- **Or skip a client entirely** — every CLI command supports `--json` and
  `subscribe` streams typed events, so a shell script is a legitimate way to
  drive pohunek.

Connecting and listing sessions is the same call in both SDKs:

First-party Rust components resolve the platform runtime path centrally. Custom
SDK clients should receive the exact local socket endpoint from configuration;
the examples use `POHUNEK_SOCKET` rather than reconstructing a Linux-only path.
With an explicit `XDG_RUNTIME_DIR`, Pohunek uses its `pohunek` child on Linux and
macOS. Linux requires that variable, while macOS without it uses
`/private/tmp/pohunek-<effective-uid>` and ignores `TMPDIR` for this decision.
`@pohunek/sdk` derives the same default and checks the runtime directory
before connecting.

```rust
// Rust — `pohunek-client`
use pohunek_client::{protocol::method::SessionList, Client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = std::env::var_os("POHUNEK_SOCKET")
        .ok_or_else(|| anyhow::anyhow!("set POHUNEK_SOCKET to the daemon socket path"))?;
    // Local daemon over its owner-only Unix socket.
    let mut client = Client::connect_local(&sock).await?;
    // ...or a remote host over the NetBird/WireGuard mesh:
    // let mut client = Client::connect("workstation", &sock).await?;

    let sessions = client.call::<SessionList>(Default::default()).await?;
    for s in sessions {
        println!("{} {:?} {:?}", s.id.0, s.state, s.activity);
    }
    Ok(())
}
```

```ts
// TypeScript — `@pohunek/sdk`
import { connectLocal } from "@pohunek/sdk";

const sock = process.env.POHUNEK_SOCKET;
if (!sock) throw new Error("set POHUNEK_SOCKET to the daemon socket path");
const client = await connectLocal(sock);

const sessions = await client.call("session.list", {});
for (const s of sessions) {
  console.log(s.id, s.state, s.activity);
}
await client.close();
```

Browsers use the node-free entry and reach a daemon through a WebSocket relay origin:

```ts
import { Client } from "@pohunek/sdk/browser";

const client = await Client.connectWs(window.location.origin, "workstation");
```

Or subscribe to the same live event stream the CLI and other clients consume — session
lifecycle, agent state, and notifications, decoded into typed events:

```rust
// Rust — subscribe consumes the client and hands back an event stream.
use pohunek_client::{next_request_id, protocol::{method, Request}, Client};
use serde_json::Value;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sock = std::env::var_os("POHUNEK_SOCKET")
        .ok_or_else(|| anyhow::anyhow!("set POHUNEK_SOCKET to the daemon socket path"))?;
    let client = Client::connect_local(&sock).await?;

    let request = Request::new(next_request_id(method::SUBSCRIBE), method::SUBSCRIBE, Value::Null);
    let mut events = client.subscribe(&request).await?;
    // Runs until the daemon closes the stream.
    while let Some(ev) = events.next_event().await? {
        // `event` is the name (e.g. "agent_state"); `payload` is the flattened JSON body.
        println!("{}: {}", ev.event, ev.payload);
    }
    Ok(())
}
```

```ts
// TypeScript — same event stream.
import { connectLocal, nextRequestId } from "@pohunek/sdk";
import { PROTOCOL_VERSION } from "@pohunek/protocol";

const client = await connectLocal(sock);
const subscription = await client.subscribe({
  v: PROTOCOL_VERSION,
  id: nextRequestId("subscribe"),
  method: "subscribe",
  params: null,
});

for (let ev = await subscription.nextEvent(); ev !== null; ev = await subscription.nextEvent()) {
  console.log(ev.event, ev);
}
```

To follow agents across turns without polling, wait for the next turn event of
one or more sessions and chain the returned cursor (Rust
`Client::session_wait_turn`, TypeScript `client.sessionWaitTurn`). A timeout
still returns per-session progress, so the caller can tell "still working" from
"stuck"; see [Turn Waits](public-api.md#turn-waits).

```ts
// TypeScript — one bounded wait; pass `result.cursor` as `after` next time.
const result = await client.sessionWaitTurn({
  session_ids: ["s-41"],
  until: ["turn_completed", "agent_blocked", "approval_required", "exited"],
  timeout_ms: 300_000,
});
console.log(result.reason, result.event, result.progress);
```

A waited input (Rust `Client::session_input`, TypeScript `client.sessionInput`)
confirms that the agent started on the text when its `wait.until` names
`working`. Set `wait.allow_prompt_accepted` to let a Claude session's
prompt-submit hook answer that wait when no `working` activity was observed. The
result then has `prompt_accepted: true` and no `activity`, `activity_source` or
`activity_revision`; the SDK helpers accept that shape only from a caller that
opted in and asked for `working`, and reject it otherwise. A daemon that
predates the option answers on activity alone, so `prompt_accepted` is absent.

```ts
// TypeScript — `working` is answered by activity or by the prompt-submit hook.
const input = await client.sessionInput({
  session_id: "s-41",
  text: "run the tests",
  wait: { until: ["working"], timeout_ms: 5_000, allow_prompt_accepted: true },
});
console.log(input.prompt_accepted ?? false, input.activity);
```
