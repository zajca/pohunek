import { createConnection, type Socket } from "node:net";
import { MAX_CONTROL_LINE_BYTES } from "@pohunek/protocol";
import { validateRelayBindAddr } from "./bind";

export type DaemonTarget =
  | { readonly kind: "unix"; readonly socketPath: string }
  | { readonly kind: "tcp"; readonly host: string; readonly port: number };

export interface StartRelayOptions {
  readonly bindHost: string;
  readonly port: number;
  readonly targets: ReadonlyMap<string, DaemonTarget>;
  readonly allowLoopbackBind?: boolean;
}

export interface RelayHandle {
  readonly url: string;
  readonly port: number;
  close(): Promise<void>;
}

type RelayMode = "control" | "attach";

interface RelayRoute {
  readonly host: string;
  readonly mode: RelayMode;
}

interface RelayWebSocketData {
  readonly host: string;
  readonly mode: RelayMode;
  readonly target: DaemonTarget;
  socket?: Socket;
  controlBuffer: number[];
  closed: boolean;
  // Serializes writes to the daemon socket. Each frame is chained after the
  // previous write resolves, so only one `drain` waiter is ever registered on
  // the socket at a time. Without this, a burst of attach/control frames adds
  // one `once("drain")` listener each (unbounded listener growth + unbounded
  // socket buffering); chaining bounds both to a single in-flight write.
  writeChain: Promise<void>;
}

interface BunServeOptions<TData> {
  readonly hostname: string;
  readonly port: number;
  fetch(request: Request, server: BunServer<TData>): Response | undefined | Promise<Response | undefined>;
  readonly websocket: {
    open(ws: BunServerWebSocket<TData>): void;
    message(ws: BunServerWebSocket<TData>, message: BunWebSocketMessage): void;
    close(ws: BunServerWebSocket<TData>): void;
  };
}

interface BunServer<TData> {
  readonly port: number;
  upgrade(request: Request, options: { readonly data: TData }): boolean;
  stop(force?: boolean): void;
}

interface BunServerWebSocket<TData> {
  readonly data: TData;
  readonly readyState: number;
  send(message: string | Uint8Array): number | boolean;
  close(code?: number, reason?: string): void;
}

interface BunRuntime {
  serve<TData>(options: BunServeOptions<TData>): BunServer<TData>;
}

type BunWebSocketMessage = string | ArrayBuffer | Uint8Array;

const WEBSOCKET_OPEN_STATE = 1;
const NORMAL_CLOSE_CODE = 1000;
const PROTOCOL_CLOSE_CODE = 1002;
const UNSUPPORTED_DATA_CLOSE_CODE = 1003;
const INVALID_TEXT_CLOSE_CODE = 1007;
const POLICY_CLOSE_CODE = 1008;
const INTERNAL_CLOSE_CODE = 1011;
const LINE_FEED = 0x0a;
const CONTROL_FRAME_DELIMITER = Uint8Array.of(LINE_FEED);

const RESPONSE_NOT_FOUND = new Response("unknown relay target", { status: 404 });
const RESPONSE_UPGRADE_REQUIRED = new Response("websocket upgrade required", { status: 426 });
const RESPONSE_BAD_UPGRADE = new Response("websocket upgrade failed", { status: 400 });

const decoder = new TextDecoder("utf-8", { fatal: true });
const encoder = new TextEncoder();

export function startRelay(options: StartRelayOptions): Promise<RelayHandle> {
  try {
    validateRelayBindAddr(
      options.bindHost,
      options.allowLoopbackBind === undefined ? {} : { allowLoopback: options.allowLoopbackBind },
    );
    const bun = bunRuntime();
    const targets = new Map(options.targets);

    const server = bun.serve<RelayWebSocketData>({
      hostname: options.bindHost,
      port: options.port,
      fetch(request, upgradeServer): Response | undefined {
        const route = parseRelayRoute(request.url);
        if (route === undefined) {
          return RESPONSE_NOT_FOUND;
        }

        const target = targets.get(route.host);
        if (target === undefined) {
          return RESPONSE_NOT_FOUND;
        }

        if (!isWebSocketUpgrade(request)) {
          return RESPONSE_UPGRADE_REQUIRED;
        }

        const upgraded = upgradeServer.upgrade(request, {
          data: {
            host: route.host,
            mode: route.mode,
            target,
            controlBuffer: [],
            closed: false,
            writeChain: Promise.resolve(),
          },
        });
        return upgraded ? undefined : RESPONSE_BAD_UPGRADE;
      },
      websocket: {
        open(ws): void {
          openTunnel(ws);
        },
        message(ws, message): void {
          handleClientMessage(ws, message);
        },
        close(ws): void {
          closeTunnel(ws.data, NORMAL_CLOSE_CODE, "websocket closed");
        },
      },
    });

    return Promise.resolve({
      url: `http://${formatUrlHost(options.bindHost)}:${server.port}`,
      port: server.port,
      close: (): Promise<void> => {
        server.stop(true);
        return Promise.resolve();
      },
    });
  } catch (error: unknown) {
    return Promise.reject(error instanceof Error ? error : new Error(String(error)));
  }
}

function openTunnel(ws: BunServerWebSocket<RelayWebSocketData>): void {
  const socket = dialTarget(ws.data.target);
  ws.data.socket = socket;

  socket.on("data", (chunk: Buffer): void => {
    if (ws.data.mode === "control") {
      relayControlBytes(ws, chunk);
      return;
    }
    sendBinaryFrame(ws, new Uint8Array(chunk));
  });
  socket.once("error", (): void => {
    closeTunnel(ws.data, INTERNAL_CLOSE_CODE, "daemon connection failed", ws);
  });
  socket.once("end", (): void => {
    closeTunnel(ws.data, NORMAL_CLOSE_CODE, "daemon connection ended", ws);
  });
  socket.once("close", (): void => {
    closeTunnel(ws.data, NORMAL_CLOSE_CODE, "daemon connection closed", ws);
  });
}

function handleClientMessage(
  ws: BunServerWebSocket<RelayWebSocketData>,
  message: BunWebSocketMessage,
): void {
  if (ws.data.mode === "control") {
    handleControlFrame(ws, message);
    return;
  }
  handleAttachFrame(ws, message);
}

function handleControlFrame(
  ws: BunServerWebSocket<RelayWebSocketData>,
  message: BunWebSocketMessage,
): void {
  if (typeof message !== "string") {
    closeTunnel(ws.data, PROTOCOL_CLOSE_CODE, "control frames must be text", ws);
    return;
  }
  if (message.includes("\n")) {
    closeTunnel(ws.data, PROTOCOL_CLOSE_CODE, "control frame contained newline", ws);
    return;
  }

  const bytes = encoder.encode(message);
  if (bytes.byteLength > MAX_CONTROL_LINE_BYTES) {
    closeTunnel(ws.data, POLICY_CLOSE_CODE, "control line too large", ws);
    return;
  }

  const socket = ws.data.socket;
  if (socket === undefined || socket.destroyed) {
    closeTunnel(ws.data, INTERNAL_CLOSE_CODE, "daemon connection unavailable", ws);
    return;
  }

  queueDaemonWrite(ws, socket, [bytes, CONTROL_FRAME_DELIMITER]);
}

function handleAttachFrame(
  ws: BunServerWebSocket<RelayWebSocketData>,
  message: BunWebSocketMessage,
): void {
  if (typeof message === "string") {
    closeTunnel(ws.data, UNSUPPORTED_DATA_CLOSE_CODE, "attach frames must be binary", ws);
    return;
  }

  const socket = ws.data.socket;
  if (socket === undefined || socket.destroyed) {
    closeTunnel(ws.data, INTERNAL_CLOSE_CODE, "daemon connection unavailable", ws);
    return;
  }

  const bytes = bytesFromBinaryFrame(message);
  queueDaemonWrite(ws, socket, [bytes]);
}

// Chain a daemon-socket write after all prior writes for this tunnel, so at
// most one write (and thus one `drain` listener) is in flight at a time.
function queueDaemonWrite(
  ws: BunServerWebSocket<RelayWebSocketData>,
  socket: Socket,
  chunks: readonly Uint8Array[],
): void {
  ws.data.writeChain = ws.data.writeChain
    .then(async () => {
      if (ws.data.closed || socket.destroyed) {
        return;
      }
      for (const chunk of chunks) {
        await writeBytes(socket, chunk);
      }
    })
    .catch(() => {
      closeTunnel(ws.data, INTERNAL_CLOSE_CODE, "daemon write failed", ws);
    });
}

function relayControlBytes(
  ws: BunServerWebSocket<RelayWebSocketData>,
  chunk: Uint8Array,
): void {
  for (const byte of chunk) {
    if (byte === LINE_FEED) {
      flushControlLine(ws);
      continue;
    }

    ws.data.controlBuffer.push(byte);
    if (ws.data.controlBuffer.length > MAX_CONTROL_LINE_BYTES) {
      closeTunnel(ws.data, POLICY_CLOSE_CODE, "control line too large", ws);
      return;
    }
  }
}

function flushControlLine(ws: BunServerWebSocket<RelayWebSocketData>): void {
  const bytes = Uint8Array.from(ws.data.controlBuffer);
  ws.data.controlBuffer.length = 0;

  let line: string;
  try {
    line = decoder.decode(bytes);
  } catch {
    closeTunnel(ws.data, INVALID_TEXT_CLOSE_CODE, "control line was not utf-8", ws);
    return;
  }

  sendTextFrame(ws, line);
}

function sendTextFrame(ws: BunServerWebSocket<RelayWebSocketData>, line: string): void {
  if (ws.readyState === WEBSOCKET_OPEN_STATE) {
    ws.send(line);
  }
}

function sendBinaryFrame(ws: BunServerWebSocket<RelayWebSocketData>, bytes: Uint8Array): void {
  if (ws.readyState === WEBSOCKET_OPEN_STATE) {
    ws.send(bytes);
  }
}

function closeTunnel(
  data: RelayWebSocketData,
  code: number,
  reason: string,
  ws?: BunServerWebSocket<RelayWebSocketData>,
): void {
  if (data.closed) {
    return;
  }
  data.closed = true;
  data.socket?.destroy();
  if (ws !== undefined && ws.readyState === WEBSOCKET_OPEN_STATE) {
    ws.close(code, reason);
  }
}

function dialTarget(target: DaemonTarget): Socket {
  return target.kind === "unix"
    ? createConnection({ path: target.socketPath, allowHalfOpen: true })
    : createConnection({ host: target.host, port: target.port, allowHalfOpen: true });
}

function writeBytes(socket: Socket, bytes: Uint8Array): Promise<void> {
  return new Promise((resolve, reject) => {
    if (socket.destroyed) {
      reject(new Error("daemon socket is closed"));
      return;
    }

    const flushed = socket.write(bytes, (error) => {
      if (error instanceof Error) {
        reject(error);
      }
    });
    if (flushed) {
      resolve();
      return;
    }
    socket.once("drain", resolve);
  });
}

function parseRelayRoute(rawUrl: string): RelayRoute | undefined {
  const url = new URL(rawUrl);
  const parts = url.pathname.split("/").filter((part) => part.length > 0);
  if (parts.length !== 3 || parts[0] !== "daemon") {
    return undefined;
  }

  const mode = parts[2];
  if (mode !== "control" && mode !== "attach") {
    return undefined;
  }

  return {
    host: decodeURIComponent(parts[1] ?? ""),
    mode,
  };
}

function isWebSocketUpgrade(request: Request): boolean {
  return request.headers.get("upgrade")?.toLowerCase() === "websocket";
}

function bytesFromBinaryFrame(message: ArrayBuffer | Uint8Array): Uint8Array {
  if (message instanceof Uint8Array) {
    return new Uint8Array(message);
  }
  return new Uint8Array(message);
}

function formatUrlHost(host: string): string {
  return host.includes(":") ? `[${host}]` : host;
}

function bunRuntime(): BunRuntime {
  const runtime = (globalThis as typeof globalThis & { Bun?: BunRuntime }).Bun;
  if (runtime === undefined) {
    throw new Error("@pohunek/relay requires the Bun runtime");
  }
  return runtime;
}
