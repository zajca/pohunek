import { createConnection, type Socket } from "node:net";
import { MAX_CONTROL_LINE_BYTES } from "@pohunek/protocol";
import { validateRelayBindAddr } from "./bind";

export type DaemonTarget =
  | { readonly kind: "unix"; readonly socketPath: string }
  | { readonly kind: "tcp"; readonly host: string; readonly port: number };

export type DaemonTargetSource =
  | ReadonlyMap<string, DaemonTarget>
  | ((host: string) => DaemonTarget | undefined | Promise<DaemonTarget | undefined>);

export interface StartRelayOptions {
  readonly bindHost: string;
  readonly port: number;
  readonly targets: DaemonTargetSource;
  readonly allowLoopbackBind?: boolean;
  readonly httpHandler?: (request: Request) => Response | Promise<Response>;
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
  // Bytes queued client->daemon but not yet written. Bounded so a fast client
  // against a slow daemon target cannot grow the write chain without limit.
  pendingWriteBytes: number;
}

interface BunServeOptions<TData> {
  readonly hostname: string;
  readonly port: number;
  fetch(request: Request, server: BunServer<TData>): Response | undefined | Promise<Response | undefined>;
  readonly websocket: {
    open(ws: BunServerWebSocket<TData>): void;
    message(ws: BunServerWebSocket<TData>, message: BunWebSocketMessage): void;
    close(ws: BunServerWebSocket<TData>): void;
    drain(ws: BunServerWebSocket<TData>): void;
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
  getBufferedAmount(): number;
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

// daemon->client backpressure: once the WebSocket's outbound buffer exceeds this
// many bytes, stop reading from the daemon socket until the `drain` callback
// fires. This keeps the WS send buffer well under Bun's internal backpressure
// limit (past which frames are silently dropped) without ever losing data.
const WS_SEND_HIGH_WATER_BYTES = 1024 * 1024;
// client->daemon backpressure bound: cap the bytes queued but not yet written to
// the daemon socket. A fast client against a slow daemon target that exceeds
// this is closed fail-closed rather than allowed to grow memory without limit.
const DAEMON_WRITE_QUEUE_CAP_BYTES = 8 * 1024 * 1024;

const RESPONSE_NOT_FOUND = new Response("unknown relay target", { status: 404 });
const RESPONSE_UPGRADE_REQUIRED = new Response("websocket upgrade required", { status: 426 });
const RESPONSE_BAD_UPGRADE = new Response("websocket upgrade failed", { status: 400 });
const RESPONSE_TARGET_UNAVAILABLE = new Response("relay target unavailable", { status: 503 });

const decoder = new TextDecoder("utf-8", { fatal: true });
const encoder = new TextEncoder();

export function startRelay(options: StartRelayOptions): Promise<RelayHandle> {
  try {
    validateRelayBindAddr(
      options.bindHost,
      options.allowLoopbackBind === undefined ? {} : { allowLoopback: options.allowLoopbackBind },
    );
    const bun = bunRuntime();
    const targetForHost = targetResolver(options.targets);

    const server = bun.serve<RelayWebSocketData>({
      hostname: options.bindHost,
      port: options.port,
      async fetch(request, upgradeServer): Promise<Response | undefined> {
        const route = parseRelayRoute(request.url);
        if (route === undefined) {
          return await options.httpHandler?.(request) ?? RESPONSE_NOT_FOUND;
        }

        let target: DaemonTarget | undefined;
        try {
          target = await targetForHost(route.host);
        } catch {
          return RESPONSE_TARGET_UNAVAILABLE;
        }
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
            pendingWriteBytes: 0,
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
        drain(ws): void {
          // The client's WebSocket send buffer drained; resume reading daemon
          // output that daemon->client backpressure had paused.
          ws.data.socket?.resume();
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
  socket.once("error", (error: Error): void => {
    logRelayError(ws.data, "daemon connection failed", error);
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
  let queued = 0;
  for (const chunk of chunks) {
    queued += chunk.byteLength;
  }
  ws.data.pendingWriteBytes += queued;
  if (ws.data.pendingWriteBytes > DAEMON_WRITE_QUEUE_CAP_BYTES) {
    // Fail closed rather than let a fast client outrun a slow daemon target and
    // grow the write chain without bound.
    closeTunnel(ws.data, POLICY_CLOSE_CODE, "daemon write queue overflow", ws);
    return;
  }

  ws.data.writeChain = ws.data.writeChain
    .then(async () => {
      if (ws.data.closed || socket.destroyed) {
        return;
      }
      for (const chunk of chunks) {
        await writeBytes(socket, chunk);
      }
    })
    .then(
      () => {
        ws.data.pendingWriteBytes -= queued;
      },
      (error: unknown) => {
        ws.data.pendingWriteBytes -= queued;
        logRelayError(ws.data, "daemon write failed", error);
        closeTunnel(ws.data, INTERNAL_CLOSE_CODE, "daemon write failed", ws);
      },
    );
}

// Minimal structured error logging to stderr so a failing relay tunnel leaves a
// diagnosable trace (the relay is a server component; silent teardown would give
// an operator nothing to backtest against). Payload bytes are never logged.
function logRelayError(data: RelayWebSocketData, context: string, error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  console.error(
    JSON.stringify({
      level: "error",
      component: "pohunek-relay",
      host: data.host,
      mode: data.mode,
      context,
      error: message,
    }),
  );
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
    applySendBackpressure(ws);
  }
}

function sendBinaryFrame(ws: BunServerWebSocket<RelayWebSocketData>, bytes: Uint8Array): void {
  if (ws.readyState === WEBSOCKET_OPEN_STATE) {
    ws.send(bytes);
    applySendBackpressure(ws);
  }
}

// If the WebSocket's outbound buffer is filling faster than the client drains
// it, pause the daemon socket so we stop producing frames. The `drain` handler
// resumes it. Without this, Bun silently drops frames once its send buffer
// exceeds the backpressure limit, truncating attach output / control lines.
function applySendBackpressure(ws: BunServerWebSocket<RelayWebSocketData>): void {
  if (ws.getBufferedAmount() > WS_SEND_HIGH_WATER_BYTES) {
    ws.data.socket?.pause();
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

function targetResolver(
  source: DaemonTargetSource,
): (host: string) => Promise<DaemonTarget | undefined> {
  return typeof source === "function"
    ? async (host: string): Promise<DaemonTarget | undefined> => await source(host)
    : (host: string): Promise<DaemonTarget | undefined> => Promise.resolve(source.get(host));
}

function bunRuntime(): BunRuntime {
  const runtime = (globalThis as typeof globalThis & { Bun?: BunRuntime }).Bun;
  if (runtime === undefined) {
    throw new Error("@pohunek/backend requires the Bun runtime");
  }
  return runtime;
}
