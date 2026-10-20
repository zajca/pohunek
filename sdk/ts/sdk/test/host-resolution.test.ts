import { describe, expect, test } from "bun:test";
import { MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, type HostResolution } from "@pohunek/protocol";
import { ClientError, attachRaw, connectHost, resolveHost } from "@pohunek/sdk";
import {
  errResponseLine,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  startUnixDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

const PEER_ROUTE = "peer-b";
const PINNED_ROUTE = "netbird:peer~cGVlci1rZXk@18722";
const PEER_KEY = "peer-key";
const PEER_FQDN = "peer-b.overlay.test";
const NO_REQUEST_WINDOW_MS = 100;
const READY_BYTES = Uint8Array.of(0x70, 0x74, 0x79);

function remoteResolution(address: string, port: number): HostResolution {
  return {
    target: "remote",
    address,
    port,
    pinned_route: PINNED_ROUTE,
    overlay: "netbird",
    peer_id: PEER_KEY,
    fqdn: PEER_FQDN,
  };
}

/** A reply that leaves the connection open for the next request. */
function keepOpen(line: (requestLine: string) => string): ScriptStep {
  return { kind: "delay", ms: 0, line };
}

/** The version probe a client sends before a method newer than protocol 3. */
function healthReply(): ScriptStep {
  return keepOpen((requestLine) =>
    okResponseLine(requestIdFromLine(requestLine), {
      status: "ok",
      daemon_version: "local",
      protocol_version: PROTOCOL_VERSION,
    }),
  );
}

function resolveReplies(resolution: HostResolution): ScriptStep[] {
  return [
    healthReply(),
    keepOpen((requestLine) => okResponseLine(requestIdFromLine(requestLine), resolution)),
  ];
}

/** Consumes the version probe and returns the `host.resolve` request that follows. */
async function nextResolveRequest(daemon: MockDaemon): Promise<Record<string, unknown>> {
  expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
  return parseRequestLine(await daemon.nextRequest());
}

function unixPath(daemon: MockDaemon): string {
  if (daemon.endpoint.kind !== "unix") {
    throw new Error("host resolution tests require a real Unix endpoint");
  }
  return daemon.endpoint.socketPath;
}

function tcpAddress(daemon: MockDaemon): { host: string; port: number } {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("host resolution tests require a real TCP endpoint");
  }
  return { host: daemon.endpoint.host, port: daemon.endpoint.port };
}

async function failure(promise: Promise<unknown>): Promise<ClientError> {
  const error = await promise.then(
    () => undefined,
    (caught: unknown) => caught,
  );
  expect(error).toBeInstanceOf(ClientError);
  return error as ClientError;
}

describe("host resolution through the local daemon", () => {
  test("local and the empty route resolve without contacting the daemon", async () => {
    const daemon = await startUnixDaemon([]);
    try {
      expect(await resolveHost("local", unixPath(daemon))).toEqual({ target: "local" });
      expect(await resolveHost("", unixPath(daemon))).toEqual({ target: "local" });
      await daemon.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await daemon.close();
    }
  });

  test("a peer route is resolved by the daemon and keeps the typed name", async () => {
    const daemon = await startUnixDaemon(resolveReplies(remoteResolution("100.64.0.9", 18_722)));
    try {
      const resolved = await resolveHost(PEER_ROUTE, unixPath(daemon));

      expect(resolved).toEqual({
        target: "remote",
        display: PEER_ROUTE,
        address: { host: "100.64.0.9", port: 18_722 },
        pinnedRoute: PINNED_ROUTE,
        overlay: "netbird",
        peerId: PEER_KEY,
        fqdn: PEER_FQDN,
      });
      const sent = await nextResolveRequest(daemon);
      expect(sent["method"]).toBe("host.resolve");
      expect(sent["params"]).toEqual({ route: PEER_ROUTE });
    } finally {
      await daemon.close();
    }
  });

  test("a provider that reports no peer id or FQDN leaves them out", async () => {
    const bare: HostResolution = {
      target: "remote",
      address: "100.64.0.9",
      port: 18_722,
      pinned_route: PINNED_ROUTE,
      overlay: "netbird",
    };
    const daemon = await startUnixDaemon(resolveReplies(bare));
    try {
      const resolved = await resolveHost(PEER_ROUTE, unixPath(daemon));

      expect("peerId" in resolved).toBe(false);
      expect("fqdn" in resolved).toBe(false);
    } finally {
      await daemon.close();
    }
  });

  test("this machine's own overlay name resolves to local", async () => {
    const daemon = await startUnixDaemon(resolveReplies({ target: "local" }));
    try {
      expect(await resolveHost("self.overlay.test", unixPath(daemon))).toEqual({
        target: "local",
      });
    } finally {
      await daemon.close();
    }
  });

  for (const [code, msg] of [
    ["overlay_host_ambiguous", "host 'twin' exists on multiple hosts"],
    ["host_unknown", "unknown host 'nobody'"],
    ["host_identity_unpinnable", "host 'bare' reports no peer id or FQDN"],
  ] as const) {
    test(`the daemon's ${code} error keeps its code, message and hint`, async () => {
      const daemon = await startUnixDaemon([
        healthReply(),
        keepOpen((requestLine) =>
          errResponseLine(requestIdFromLine(requestLine), {
            class: "discovery",
            code,
            msg,
            recover: "run `pohunek host list`",
          }),
        ),
      ]);
      try {
        const error = await failure(resolveHost("twin", unixPath(daemon)));

        expect(error.code).toBe(code);
        expect(error.toProtocolError()).toEqual({
          class: "discovery",
          code,
          msg,
          recover: "run `pohunek host list`",
        });
      } finally {
        await daemon.close();
      }
    });
  }

  test("a daemon of the previous protocol version refuses it as too old", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) =>
          JSON.stringify({
            v: MIN_PROTOCOL_VERSION,
            id: requestIdFromLine(requestLine),
            ok: { status: "ok", daemon_version: "0.0.0", protocol_version: MIN_PROTOCOL_VERSION },
          }),
      },
    ]);
    try {
      const error = await failure(resolveHost(PEER_ROUTE, unixPath(daemon)));

      expect(error.kind).toBe("daemonProtocolTooOld");
      expect(error.code).toBe("daemon_protocol_too_old");
      // Only the version probe reached the daemon.
      expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
      await daemon.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await daemon.close();
    }
  });
});

describe("connecting by host route", () => {
  test("a remote route reaches only the resolved peer's daemon", async () => {
    const peer = await startTcpDaemon([
      {
        kind: "reply",
        line: (requestLine) =>
          okResponseLine(requestIdFromLine(requestLine), {
            status: "ok",
            daemon_version: "peer",
            protocol_version: 4,
          }),
      },
    ]);
    const { host, port } = tcpAddress(peer);
    const local = await startUnixDaemon(resolveReplies(remoteResolution(host, port)));
    try {
      const client = await connectHost(PEER_ROUTE, unixPath(local));
      await client.call("daemon.health", null);
      await client.close();

      expect(parseRequestLine(await peer.nextRequest())["method"]).toBe("daemon.health");
      // The local daemon was asked to resolve and nothing else.
      expect((await nextResolveRequest(local))["method"]).toBe("host.resolve");
      await local.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await peer.close();
      await local.close();
    }
  });

  test("local routes use the local daemon without resolving", async () => {
    const local = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) =>
          okResponseLine(requestIdFromLine(requestLine), {
            status: "ok",
            daemon_version: "local",
            protocol_version: 4,
          }),
      },
    ]);
    try {
      const client = await connectHost("local", unixPath(local));
      await client.call("daemon.health", null);
      await client.close();

      expect(parseRequestLine(await local.nextRequest())["method"]).toBe("daemon.health");
      await local.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await local.close();
    }
  });

  test("a remote attach is redeemed on the resolved peer", async () => {
    const peer = await startTcpDaemon([{ kind: "attachSuccess", emit: [READY_BYTES] }]);
    const { host, port } = tcpAddress(peer);
    const local = await startUnixDaemon(resolveReplies(remoteResolution(host, port)));
    try {
      const raw = await attachRaw(PEER_ROUTE, unixPath(local), "stream-remote");

      expect(new TextDecoder().decode(await peer.nextAttachPrelude())).toBe(
        '{"attach":"stream-remote"}\n',
      );
      await raw.close();
      expect((await nextResolveRequest(local))["method"]).toBe("host.resolve");
      await local.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await peer.close();
      await local.close();
    }
  });

  test("a local attach is redeemed on the local daemon", async () => {
    const local = await startUnixDaemon([{ kind: "attachSuccess", emit: [READY_BYTES] }]);
    try {
      const raw = await attachRaw("local", unixPath(local), "stream-local");

      expect(new TextDecoder().decode(await local.nextAttachPrelude())).toBe(
        '{"attach":"stream-local"}\n',
      );
      await raw.close();
    } finally {
      await local.close();
    }
  });

  test("an unreachable local daemon fails the attach without dialing a peer", async () => {
    const peer = await startTcpDaemon([]);
    const local = await startUnixDaemon([]);
    const socketPath = unixPath(local);
    await local.close();
    try {
      const error = await failure(attachRaw(PEER_ROUTE, socketPath, "stream-none"));

      expect(error.kind).toBe("daemonUnreachable");
      await peer.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await peer.close();
    }
  });

  test("a failed resolution connects to nothing", async () => {
    const peer = await startTcpDaemon([]);
    const local = await startUnixDaemon([
      healthReply(),
      keepOpen((requestLine) =>
        errResponseLine(requestIdFromLine(requestLine), {
          class: "discovery",
          code: "host_unknown",
          msg: "unknown host 'nobody'",
        }),
      ),
    ]);
    try {
      const error = await failure(attachRaw("nobody", unixPath(local), "stream-none"));

      expect(error.code).toBe("host_unknown");
      await peer.expectNoRequest(NO_REQUEST_WINDOW_MS);
    } finally {
      await peer.close();
      await local.close();
    }
  });
});
