import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";
import { startTestRelay, type TestRelayHandle } from "@pohunek/testkit/bun-relay";

const LOOPBACK_HOST = "127.0.0.1";
const RELAY_HOST = "local";
const WS_TIMEOUT_MS = 5000;
const WS_NORMAL_CLOSE = 1000;
const WS_PROTOCOL_ERROR = 1002;
const WS_UNSUPPORTED_DATA = 1003;
const HTTP_NOT_FOUND = 404;
const HTTP_UPGRADE_REQUIRED = 426;

interface EchoDaemon {
  readonly socketPath: string;
  close(): Promise<void>;
}

// A real unix-socket daemon stand-in: it echoes every byte back, which is
// enough to observe both framing modes end to end.
async function startEchoDaemon(): Promise<EchoDaemon> {
  const dir = mkdtempSync(join(tmpdir(), "pk-relay-"));
  const socketPath = join(dir, "echo.sock");
  const sockets = new Set<Socket>();
  const server: Server = createServer((socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => undefined);
    socket.on("data", (chunk) => socket.write(chunk));
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, resolve);
  });
  return {
    socketPath,
    close: async (): Promise<void> => {
      for (const socket of sockets) {
        socket.destroy();
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

async function withRelay(
  run: (relay: TestRelayHandle, daemon: EchoDaemon) => Promise<void>,
): Promise<void> {
  const daemon = await startEchoDaemon();
  const relay = await startTestRelay({
    bindHost: LOOPBACK_HOST,
    port: 0,
    targets: new Map([[RELAY_HOST, { kind: "unix", socketPath: daemon.socketPath }]]),
  });
  try {
    await run(relay, daemon);
  } finally {
    await relay.close();
    await daemon.close();
  }
}

function openSocket(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    const timer = setTimeout(() => reject(new Error("websocket open timed out")), WS_TIMEOUT_MS);
    ws.addEventListener("open", () => {
      clearTimeout(timer);
      resolve(ws);
    });
    ws.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error("websocket failed to open"));
    });
  });
}

function nextMessage(ws: WebSocket): Promise<string | ArrayBuffer> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket message timed out")), WS_TIMEOUT_MS);
    ws.addEventListener(
      "message",
      (event) => {
        clearTimeout(timer);
        resolve(event.data as string | ArrayBuffer);
      },
      { once: true },
    );
  });
}

function closeCode(ws: WebSocket): Promise<number> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket close timed out")), WS_TIMEOUT_MS);
    ws.addEventListener(
      "close",
      (event) => {
        clearTimeout(timer);
        resolve(event.code);
      },
      { once: true },
    );
  });
}

describe("startTestRelay", () => {
  test("refuses every non-loopback bind with a clear error", async () => {
    for (const bindHost of ["0.0.0.0", "::", "192.168.1.10", "100.64.0.1", "localhost", "127.0.0"]) {
      let message = "";
      try {
        const relay = await startTestRelay({ bindHost, port: 0, targets: new Map() });
        await relay.close();
      } catch (error: unknown) {
        message = error instanceof Error ? error.message : String(error);
      }
      expect(message).toContain("only binds to loopback");
    }
  });

  test("rejects non-WebSocket relay requests before resolving a target", async () => {
    let resolutionCount = 0;
    const relay = await startTestRelay({
      bindHost: LOOPBACK_HOST,
      port: 0,
      targets: (): undefined => {
        resolutionCount += 1;
        return undefined;
      },
    });
    try {
      const response = await fetch(`${relay.url}/daemon/remote/control`);
      expect(response.status).toBe(HTTP_UPGRADE_REQUIRED);
      expect(resolutionCount).toBe(0);
    } finally {
      await relay.close();
    }
  });

  test("answers 404 for paths outside the relay routes and for unknown hosts", async () => {
    await withRelay(async (relay) => {
      for (const path of ["/", "/daemon/local", "/daemon/local/other", "/other/local/control"]) {
        const response = await fetch(`${relay.url}${path}`);
        expect(response.status).toBe(HTTP_NOT_FOUND);
      }
      const unknownHost = await fetch(`${relay.url}/daemon/missing/control`, {
        headers: { upgrade: "websocket" },
      });
      expect(unknownHost.status).toBe(HTTP_NOT_FOUND);
    });
  });

  test("control mode forwards a text frame as one newline-terminated line and returns it as text", async () => {
    await withRelay(async (relay) => {
      const ws = await openSocket(`${relay.url.replace("http", "ws")}/daemon/${RELAY_HOST}/control`);
      const reply = nextMessage(ws);
      ws.send('{"id":1}');
      expect(await reply).toBe('{"id":1}');
      ws.close();
    });
  });

  test("control mode closes the tunnel on binary frames and on embedded newlines", async () => {
    await withRelay(async (relay) => {
      const binary = await openSocket(`${relay.url.replace("http", "ws")}/daemon/${RELAY_HOST}/control`);
      const binaryClosed = closeCode(binary);
      binary.send(Uint8Array.of(1, 2, 3));
      expect(await binaryClosed).toBe(WS_PROTOCOL_ERROR);

      const newline = await openSocket(`${relay.url.replace("http", "ws")}/daemon/${RELAY_HOST}/control`);
      const newlineClosed = closeCode(newline);
      newline.send("a\nb");
      expect(await newlineClosed).toBe(WS_PROTOCOL_ERROR);
    });
  });

  test("attach mode relays binary frames both ways and rejects text frames", async () => {
    await withRelay(async (relay) => {
      const ws = await openSocket(`${relay.url.replace("http", "ws")}/daemon/${RELAY_HOST}/attach`);
      const reply = nextMessage(ws);
      ws.send(Uint8Array.of(0x70, 0x74, 0x79));
      const received = await reply;
      expect(received instanceof ArrayBuffer).toBe(true);
      expect(Array.from(new Uint8Array(received as ArrayBuffer))).toEqual([0x70, 0x74, 0x79]);

      const closed = closeCode(ws);
      ws.send("text");
      expect(await closed).toBe(WS_UNSUPPORTED_DATA);
    });
  });

  test("closes the WebSocket when the daemon connection ends", async () => {
    await withRelay(async (relay, daemon) => {
      const ws = await openSocket(`${relay.url.replace("http", "ws")}/daemon/${RELAY_HOST}/attach`);
      const reply = nextMessage(ws);
      ws.send(Uint8Array.of(1));
      await reply;
      const closed = closeCode(ws);
      await daemon.close();
      expect(await closed).toBe(WS_NORMAL_CLOSE);
    });
  });
});
