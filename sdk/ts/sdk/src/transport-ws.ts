import { MAX_CONTROL_LINE_BYTES } from "@pohunek/protocol";
import { ClientError } from "./error";
import type { ConnectOptions, ControlChannel, RawDuplex, ResolvedConnectOptions, Transport } from "./transport";
import { resolveConnectOptions } from "./transport";

export class WsTransport implements Transport {
  private readonly baseUrl: string;
  private readonly host: string;
  private readonly options: ResolvedConnectOptions;

  private constructor(baseUrl: string, host: string, options?: ConnectOptions) {
    this.baseUrl = baseUrl;
    this.host = host;
    this.options = resolveConnectOptions(options);
  }

  public static relay(baseUrl: string, host: string, options?: ConnectOptions): WsTransport {
    return new WsTransport(baseUrl, host, options);
  }

  public async control(): Promise<ControlChannel> {
    const ws = await openWebSocket(
      relayUrl(this.baseUrl, this.host, "control"),
      this.host,
      this.options.connectTimeoutMs,
    );
    return new WsControlChannel(ws);
  }

  public async raw(): Promise<RawDuplex> {
    const ws = await openWebSocket(
      relayUrl(this.baseUrl, this.host, "attach"),
      this.host,
      this.options.connectTimeoutMs,
      "arraybuffer",
    );
    return wsRawDuplex(ws);
  }
}

type RelayMode = "control" | "attach";

const WEBSOCKET_CONNECTING_STATE = 0;
const WEBSOCKET_OPEN_STATE = 1;
const WEBSOCKET_CLOSING_STATE = 2;
const WEBSOCKET_CLOSED_STATE = 3;
const NORMAL_CLOSE_CODE = 1000;
const POLICY_CLOSE_CODE = 1008;
const UNSUPPORTED_DATA_CLOSE_CODE = 1003;
const SEND_BUFFER_HIGH_WATER_BYTES = 1024 * 1024;
const SEND_BUFFER_POLL_MS = 4;
// WS has no receive-side pause API. Keep a bounded in-process buffer so a slow
// stream reader cannot accumulate unbounded attach output if the browser keeps
// receiving frames.
const RAW_RECEIVE_HIGH_WATER_BYTES = 16 * 1024 * 1024;
const RAW_WRITABLE_HIGH_WATER_BYTES = 64 * 1024;
const RAW_READABLE_HIGH_WATER_BYTES = 1024 * 1024;
const CONTROL_TEXT_ENCODER = new TextEncoder();

class WsControlChannel implements ControlChannel {
  public readonly lines: AsyncIterable<string>;
  private readonly ws: WebSocket;
  private readonly queue = new AsyncQueue<string>();

  public constructor(ws: WebSocket) {
    this.ws = ws;
    this.lines = this.queue;

    ws.addEventListener("message", (event: MessageEvent): void => {
      this.handleMessage(event.data);
    });
    ws.addEventListener("close", (): void => {
      this.queue.close();
    });
    ws.addEventListener("error", (): void => {
      this.queue.fail(ClientError.io("websocket error"));
    });
  }

  public async send(line: string): Promise<void> {
    const bytes = CONTROL_TEXT_ENCODER.encode(line);
    if (bytes.byteLength > MAX_CONTROL_LINE_BYTES) {
      throw ClientError.framing("control line exceeded maximum length");
    }
    await sendWithBackpressure(this.ws, line);
  }

  public close(): Promise<void> {
    closeWebSocket(this.ws, NORMAL_CLOSE_CODE, "control channel closed");
    this.queue.close();
    return Promise.resolve();
  }

  private handleMessage(data: unknown): void {
    if (typeof data !== "string") {
      closeWebSocket(this.ws, UNSUPPORTED_DATA_CLOSE_CODE, "control frames must be text");
      this.queue.fail(ClientError.framing("control WebSocket received a non-text frame"));
      return;
    }

    const bytes = CONTROL_TEXT_ENCODER.encode(data);
    if (bytes.byteLength > MAX_CONTROL_LINE_BYTES) {
      closeWebSocket(this.ws, POLICY_CLOSE_CODE, "control line too large");
      this.queue.fail(ClientError.framing("control line exceeded maximum length"));
      return;
    }

    this.queue.push(data);
  }
}

function wsRawDuplex(ws: WebSocket): RawDuplex {
  const receiver = new RawReceiveBuffer(ws);

  const writable = new WritableStream<Uint8Array>(
    {
      async write(chunk): Promise<void> {
        await sendWithBackpressure(ws, chunk);
      },
      close(): Promise<void> {
        // Do not close the WebSocket here. Bun does not reliably preserve the
        // read side after a half-close, and WebSocket has no half-close anyway;
        // callers use RawDuplex.close() or session.detach to end the stream.
        return Promise.resolve();
      },
      abort(): void {
        closeWebSocket(ws, NORMAL_CLOSE_CODE, "raw stream aborted");
      },
    },
    {
      highWaterMark: RAW_WRITABLE_HIGH_WATER_BYTES,
      size: (chunk): number => chunk.byteLength,
    },
  );

  return {
    readable: receiver.readable,
    writable,
    close: (): Promise<void> => {
      receiver.close();
      closeWebSocket(ws, NORMAL_CLOSE_CODE, "raw stream closed");
      return Promise.resolve();
    },
  };
}

class RawReceiveBuffer {
  public readonly readable: ReadableStream<Uint8Array>;
  private readonly ws: WebSocket;
  private readonly pending: Uint8Array[] = [];
  private controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  private pendingBytes = 0;
  private closed = false;
  private ended = false;

  public constructor(ws: WebSocket) {
    this.ws = ws;
    this.readable = new ReadableStream<Uint8Array>(
      {
        start: (controller): void => {
          this.controller = controller;
        },
        pull: (): void => {
          this.drain();
        },
        cancel: (): void => {
          this.close();
          closeWebSocket(this.ws, NORMAL_CLOSE_CODE, "raw readable canceled");
        },
      },
      {
        highWaterMark: RAW_READABLE_HIGH_WATER_BYTES,
        size: (chunk): number => chunk.byteLength,
      },
    );

    ws.addEventListener("message", (event: MessageEvent): void => {
      void this.handleMessage(event.data);
    });
    ws.addEventListener("close", (): void => {
      this.ended = true;
      this.drain();
    });
    ws.addEventListener("error", (): void => {
      this.fail(ClientError.io("websocket error"));
    });
  }

  public close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.pending.length = 0;
    this.pendingBytes = 0;
    this.controller?.close();
  }

  private async handleMessage(data: unknown): Promise<void> {
    const bytes = await bytesFromBinaryMessage(data);
    if (bytes === undefined) {
      closeWebSocket(this.ws, UNSUPPORTED_DATA_CLOSE_CODE, "attach frames must be binary");
      this.fail(ClientError.framing("raw WebSocket received a non-binary frame"));
      return;
    }
    this.push(bytes);
  }

  private push(bytes: Uint8Array): void {
    if (this.closed) {
      return;
    }

    if (this.pending.length === 0 && this.canEnqueueNow()) {
      this.controller?.enqueue(bytes);
      return;
    }

    this.pending.push(bytes);
    this.pendingBytes += bytes.byteLength;
    if (this.pendingBytes > RAW_RECEIVE_HIGH_WATER_BYTES) {
      closeWebSocket(this.ws, POLICY_CLOSE_CODE, "raw receive buffer full");
      this.fail(ClientError.framing("raw WebSocket receive buffer exceeded high-water mark"));
    }
  }

  private drain(): void {
    if (this.closed) {
      return;
    }

    while (this.pending.length > 0 && this.canEnqueueNow()) {
      const chunk = this.pending.shift();
      if (chunk === undefined) {
        break;
      }
      this.pendingBytes -= chunk.byteLength;
      this.controller?.enqueue(chunk);
    }

    if (this.ended && this.pending.length === 0) {
      this.close();
    }
  }

  private fail(error: ClientError): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.pending.length = 0;
    this.pendingBytes = 0;
    this.controller?.error(error);
  }

  private canEnqueueNow(): boolean {
    const desiredSize = this.controller?.desiredSize;
    return desiredSize === undefined || desiredSize === null || desiredSize > 0;
  }
}

class AsyncQueue<T> implements AsyncIterable<T>, AsyncIterator<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve(value: IteratorResult<T>): void;
    reject(error: unknown): void;
  }> = [];
  private closed = false;
  private failure: Error | undefined;

  public [Symbol.asyncIterator](): AsyncIterator<T> {
    return this;
  }

  public next(): Promise<IteratorResult<T>> {
    const value = this.values.shift();
    if (value !== undefined) {
      return Promise.resolve({ done: false, value });
    }
    if (this.failure !== undefined) {
      return Promise.reject(this.failure);
    }
    if (this.closed) {
      return Promise.resolve({ done: true, value: undefined });
    }
    return new Promise((resolve, reject) => {
      this.waiters.push({ resolve, reject });
    });
  }

  public push(value: T): void {
    if (this.closed) {
      return;
    }
    const waiter = this.waiters.shift();
    if (waiter !== undefined) {
      waiter.resolve({ done: false, value });
      return;
    }
    this.values.push(value);
  }

  public close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) {
      waiter.resolve({ done: true, value: undefined });
    }
  }

  public fail(error: unknown): void {
    if (this.closed) {
      return;
    }
    this.failure = error instanceof Error ? error : new Error(String(error));
    for (const waiter of this.waiters.splice(0)) {
      waiter.reject(this.failure);
    }
  }
}

function openWebSocket(
  url: string,
  host: string,
  timeoutMs: number,
  binaryType?: BinaryType,
): Promise<WebSocket> {
  if (typeof globalThis.WebSocket !== "function") {
    return Promise.reject(ClientError.hostUnreachable(host, "WHATWG WebSocket global is not available"));
  }

  return new Promise((resolve, reject) => {
    let ws: WebSocket;
    try {
      ws = new WebSocket(url);
    } catch (error: unknown) {
      reject(ClientError.hostUnreachable(host, error));
      return;
    }
    if (binaryType !== undefined) {
      ws.binaryType = binaryType;
    }

    let settled = false;
    const timer = setTimeout(() => {
      fail(new Error(`websocket connect timed out after ${timeoutMs}ms`));
    }, timeoutMs);

    const cleanup = (): void => {
      clearTimeout(timer);
      ws.removeEventListener("open", onOpen);
      ws.removeEventListener("error", onError);
      ws.removeEventListener("close", onClose);
    };
    const onOpen = (): void => {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      resolve(ws);
    };
    const onError = (): void => {
      fail(new Error("websocket connection failed"));
    };
    const onClose = (event: CloseEvent): void => {
      fail(new Error(`websocket closed before opening (${event.code})`));
    };
    const fail = (error: Error): void => {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      closeWebSocket(ws, NORMAL_CLOSE_CODE, "connect failed");
      reject(ClientError.hostUnreachable(host, error));
    };

    ws.addEventListener("open", onOpen);
    ws.addEventListener("error", onError);
    ws.addEventListener("close", onClose);
  });
}

async function sendWithBackpressure(ws: WebSocket, message: string | Uint8Array): Promise<void> {
  ensureOpen(ws);
  await waitForBufferedAmount(ws);
  ensureOpen(ws);
  ws.send(message);
  await waitForBufferedAmount(ws);
}

function waitForBufferedAmount(ws: WebSocket): Promise<void> {
  if (ws.bufferedAmount <= SEND_BUFFER_HIGH_WATER_BYTES) {
    return Promise.resolve();
  }

  return new Promise((resolve, reject) => {
    const poll = (): void => {
      if (ws.readyState === WEBSOCKET_CLOSING_STATE || ws.readyState === WEBSOCKET_CLOSED_STATE) {
        reject(ClientError.io("websocket is closed"));
        return;
      }
      if (ws.bufferedAmount <= SEND_BUFFER_HIGH_WATER_BYTES) {
        resolve();
        return;
      }
      setTimeout(poll, SEND_BUFFER_POLL_MS);
    };
    setTimeout(poll, SEND_BUFFER_POLL_MS);
  });
}

function ensureOpen(ws: WebSocket): void {
  if (ws.readyState !== WEBSOCKET_OPEN_STATE) {
    throw ClientError.io("websocket is closed");
  }
}

async function bytesFromBinaryMessage(data: unknown): Promise<Uint8Array | undefined> {
  if (typeof data === "string") {
    return undefined;
  }
  if (data instanceof ArrayBuffer) {
    return new Uint8Array(data);
  }
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
  }
  if (typeof Blob !== "undefined" && data instanceof Blob) {
    return new Uint8Array(await data.arrayBuffer());
  }
  return undefined;
}

function closeWebSocket(ws: WebSocket, code: number, reason: string): void {
  if (
    ws.readyState === WEBSOCKET_CONNECTING_STATE ||
    ws.readyState === WEBSOCKET_OPEN_STATE ||
    ws.readyState === WEBSOCKET_CLOSING_STATE
  ) {
    ws.close(code, reason);
  }
}

function relayUrl(baseUrl: string, host: string, mode: RelayMode): string {
  const url = new URL(baseUrl);
  if (url.protocol === "http:") {
    url.protocol = "ws:";
  } else if (url.protocol === "https:") {
    url.protocol = "wss:";
  } else if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw ClientError.hostUnreachable(host, `unsupported WebSocket relay URL protocol: ${url.protocol}`);
  }

  const basePath = url.pathname.endsWith("/") ? url.pathname.slice(0, -1) : url.pathname;
  url.pathname = `${basePath}/daemon/${encodeURIComponent(host)}/${mode}`;
  url.search = "";
  url.hash = "";
  return url.toString();
}
