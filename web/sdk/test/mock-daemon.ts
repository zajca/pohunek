import { rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION, type SessionInfo } from "@pohunek/protocol";
import { ClientError, type ControlChannel, type RawDuplex, type Transport } from "@pohunek/sdk";

export type MockEndpoint =
  | { kind: "unix"; socketPath: string }
  | { kind: "tcp"; host: string; port: number }
  | { kind: "memory"; transport: Transport };

export type ScriptStep =
  | { kind: "reply"; line: string | ((requestLine: string) => string) }
  | { kind: "garbled" }
  | { kind: "close" }
  | { kind: "oversized" }
  | { kind: "delay"; ms: number; line: string | ((requestLine: string) => string) }
  | { kind: "silent" }
  | { kind: "subscription"; ack: string | ((requestLine: string) => string); events: string[] }
  | { kind: "attachSuccess"; emit?: Uint8Array[]; echo?: boolean; readDelayMs?: number }
  | { kind: "attachFailed"; line: string | ((preludeLine: string) => string) };

export interface MockDaemon {
  endpoint: MockEndpoint;
  nextRequest(): Promise<string>;
  nextAttachPrelude(): Promise<Uint8Array>;
  expectNoRequest(timeoutMs: number): Promise<void>;
  close(): Promise<void>;
}

const LINE_FEED = 0x0a;
const CARRIAGE_RETURN = 0x0d;

let nextSocketId = 0;

export async function startUnixDaemon(steps: ScriptStep[]): Promise<MockDaemon> {
  const socketPath = join(
    tmpdir(),
    `pohunek-sdk-${process.pid}-${nextSocketId++}.sock`,
  );
  rmSync(socketPath, { force: true });
  const runtime = createRuntime(steps);
  const server = createServer({ allowHalfOpen: true }, (socket) => {
    runtime.track(socket);
    void handleSocket(socket, runtime);
  });
  const listenError = await listen(server, socketPath);
  if (listenError !== null) {
    if (isPermissionDenied(listenError)) {
      return startMemoryDaemon(steps);
    }
    throw listenError;
  }

  return {
    endpoint: { kind: "unix", socketPath },
    nextRequest: (): Promise<string> => runtime.nextRequest(),
    nextAttachPrelude: (): Promise<Uint8Array> => runtime.nextAttachPrelude(),
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {
      await closeServer(server, runtime.sockets);
      rmSync(socketPath, { force: true });
    },
  };
}

export async function startTcpDaemon(steps: ScriptStep[]): Promise<MockDaemon> {
  const runtime = createRuntime(steps);
  const server = createServer({ allowHalfOpen: true }, (socket) => {
    runtime.track(socket);
    void handleSocket(socket, runtime);
  });
  const listenError = await listen(server, { host: "127.0.0.1", port: 0 });
  if (listenError !== null) {
    if (isPermissionDenied(listenError)) {
      return startMemoryDaemon(steps);
    }
    throw listenError;
  }
  const address = server.address();
  if (!isAddressInfo(address)) {
    throw new Error("test TCP server did not bind to an address");
  }

  return {
    endpoint: { kind: "tcp", host: address.address, port: address.port },
    nextRequest: (): Promise<string> => runtime.nextRequest(),
    nextAttachPrelude: (): Promise<Uint8Array> => runtime.nextAttachPrelude(),
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {
      await closeServer(server, runtime.sockets);
    },
  };
}

export function okResponseLine(id: string, ok: unknown): string {
  return JSON.stringify({ v: PROTOCOL_VERSION, id, ok });
}

export function errResponseLine(id: string, err: unknown): string {
  return JSON.stringify({ v: PROTOCOL_VERSION, id, err });
}

export function parseRequestLine(line: string): Record<string, unknown> {
  const value: unknown = JSON.parse(line);
  if (!isRecord(value)) {
    throw new Error("request line did not decode to an object");
  }
  return value;
}

export function requestIdFromLine(line: string): string {
  const request = parseRequestLine(line);
  const id = request["id"];
  if (typeof id !== "string") {
    throw new Error("request line did not include a string id");
  }
  return id;
}

export function startMemoryDaemon(steps: ScriptStep[]): MockDaemon {
  const runtime = createRuntime(steps);
  return {
    endpoint: { kind: "memory", transport: new MemoryTransport(runtime) },
    nextRequest: (): Promise<string> => runtime.nextRequest(),
    nextAttachPrelude: (): Promise<Uint8Array> => runtime.nextAttachPrelude(),
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {},
  };
}

export function minimalSessionInfo(): SessionInfo {
  return {
    id: "s-test-1",
    capabilities: { resume: true, fork: true },
    agent: "codex",
    agent_base: "codex",
    cwd: "/workspace/pohunek",
    pid: 42424,
    cols: 120,
    rows: 32,
    state: "running",
    state_source: "process",
    created_at: "2026-07-08T00:00:00Z",
    updated_at: "2026-07-08T00:01:00Z",
  } satisfies SessionInfo;
}

interface Runtime {
  sockets: Set<Socket>;
  track(socket: Socket): void;
  nextStep(): ScriptStep;
  pushRequest(line: string): void;
  pushAttachPrelude(bytes: Uint8Array): void;
  nextRequest(): Promise<string>;
  nextAttachPrelude(): Promise<Uint8Array>;
  expectNoRequest(timeoutMs: number): Promise<void>;
}

function createRuntime(steps: ScriptStep[]): Runtime {
  const sockets = new Set<Socket>();
  const requests: string[] = [];
  const waiters: Array<(line: string) => void> = [];
  const attachPreludes: Uint8Array[] = [];
  const attachWaiters: Array<(bytes: Uint8Array) => void> = [];
  let stepIndex = 0;

  return {
    sockets,
    track(socket: Socket): void {
      sockets.add(socket);
      socket.once("close", () => {
        sockets.delete(socket);
      });
    },
    nextStep(): ScriptStep {
      const step = steps[stepIndex];
      stepIndex += 1;
      return step ?? { kind: "close" };
    },
    pushRequest(line: string): void {
      const waiter = waiters.shift();
      if (waiter !== undefined) {
        waiter(line);
        return;
      }
      requests.push(line);
    },
    pushAttachPrelude(bytes: Uint8Array): void {
      const copy = new Uint8Array(bytes);
      const waiter = attachWaiters.shift();
      if (waiter !== undefined) {
        waiter(copy);
        return;
      }
      attachPreludes.push(copy);
    },
    nextRequest(): Promise<string> {
      const line = requests.shift();
      if (line !== undefined) {
        return Promise.resolve(line);
      }
      return new Promise((resolve) => {
        waiters.push(resolve);
      });
    },
    nextAttachPrelude(): Promise<Uint8Array> {
      const bytes = attachPreludes.shift();
      if (bytes !== undefined) {
        return Promise.resolve(bytes);
      }
      return new Promise((resolve) => {
        attachWaiters.push(resolve);
      });
    },
    async expectNoRequest(timeoutMs: number): Promise<void> {
      const maybeLine = requests.shift();
      if (maybeLine !== undefined) {
        throw new Error(`unexpected request line: ${maybeLine}`);
      }
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(resolve, timeoutMs);
        waiters.push((line) => {
          clearTimeout(timer);
          reject(new Error(`unexpected request line: ${line}`));
        });
      });
    },
  };
}

class MemoryTransport implements Transport {
  private readonly runtime: Runtime;

  public constructor(runtime: Runtime) {
    this.runtime = runtime;
  }

  public control(): Promise<ControlChannel> {
    return Promise.resolve(new MemoryControlChannel(this.runtime));
  }

  public raw(): Promise<RawDuplex> {
    return Promise.resolve(createMemoryRawDuplex(this.runtime));
  }
}

function createMemoryRawDuplex(runtime: Runtime): RawDuplex {
  let readableController: ReadableStreamDefaultController<Uint8Array> | undefined;
  let preludeBuffer: Uint8Array<ArrayBufferLike> = new Uint8Array();
  let attachStep: Extract<ScriptStep, { kind: "attachSuccess" | "attachFailed" }> | undefined;
  let closed = false;

  const readable = new ReadableStream<Uint8Array>(
    {
      start(controller): void {
        readableController = controller;
      },
    },
    { highWaterMark: 1, size: (chunk): number => chunk.byteLength },
  );

  const closeReadable = (): void => {
    if (!closed) {
      closed = true;
      readableController?.close();
    }
  };

  const enqueue = (bytes: Uint8Array): void => {
    if (!closed) {
      readableController?.enqueue(new Uint8Array(bytes));
    }
  };

  const writable = new WritableStream<Uint8Array>(
    {
      async write(chunk): Promise<void> {
        if (closed) {
          return;
        }

        let rawOffset = 0;
        if (attachStep === undefined) {
          const previousPreludeLength = preludeBuffer.byteLength;
          const joined = appendBytes(preludeBuffer, chunk);
          const newlineIndex = indexOfByte(joined, LINE_FEED);
          if (newlineIndex < 0) {
            preludeBuffer = joined;
            return;
          }

          const rawLine = joined.subarray(0, newlineIndex + 1);
          runtime.pushAttachPrelude(rawLine);
          const line = decodeLine(joined.subarray(0, newlineIndex));
          const step = runtime.nextStep();
          if (step.kind !== "attachSuccess" && step.kind !== "attachFailed") {
            throw new Error("memory raw transport expected an attach script step");
          }
          attachStep = step;
          preludeBuffer = new Uint8Array();
          rawOffset = newlineIndex + 1 - previousPreludeLength;

          if (attachStep.kind === "attachFailed") {
            enqueue(new TextEncoder().encode(`${resolveLine(attachStep.line, line)}\n`));
            closeReadable();
            return;
          }

          for (const bytes of attachStep.emit ?? []) {
            enqueue(bytes);
          }
        }

        if (attachStep.kind === "attachSuccess") {
          const rawChunk = chunk.subarray(Math.max(0, rawOffset));
          if (rawChunk.byteLength > 0) {
            await handleMemoryRawChunk(rawChunk, attachStep, enqueue);
          }
        }
      },
      close(): void {
        closeReadable();
      },
      abort(): void {
        closeReadable();
      },
    },
    { highWaterMark: 1, size: (chunk): number => chunk.byteLength },
  );

  return {
    readable,
    writable,
    close: (): Promise<void> => {
      closeReadable();
      return Promise.resolve();
    },
  };
}

async function handleMemoryRawChunk(
  chunk: Uint8Array,
  step: Extract<ScriptStep, { kind: "attachSuccess" }>,
  enqueue: (bytes: Uint8Array) => void,
): Promise<void> {
  if (step.readDelayMs !== undefined) {
    await delay(step.readDelayMs);
  }
  if (step.echo === true) {
    enqueue(chunk);
  }
}

class MemoryControlChannel implements ControlChannel {
  public readonly lines: AsyncIterable<string>;
  private readonly runtime: Runtime;
  private readonly queue = new AsyncQueue<string>();

  public constructor(runtime: Runtime) {
    this.runtime = runtime;
    this.lines = this.queue;
  }

  public send(line: string): Promise<void> {
    const byteLength = new TextEncoder().encode(line).byteLength;
    if (byteLength > MAX_CONTROL_LINE_BYTES) {
      return Promise.reject(ClientError.framing("control line exceeded maximum length"));
    }
    this.runtime.pushRequest(line);
    applyMemoryStep(this.queue, line, this.runtime.nextStep());
    return Promise.resolve();
  }

  public close(): Promise<void> {
    this.queue.close();
    return Promise.resolve();
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
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) {
      waiter.resolve({ done: true, value: undefined });
    }
  }

  public fail(error: unknown): void {
    this.failure = error instanceof Error ? error : new Error(String(error));
    for (const waiter of this.waiters.splice(0)) {
      waiter.reject(this.failure);
    }
  }
}

function applyMemoryStep(queue: AsyncQueue<string>, requestLine: string, step: ScriptStep): void {
  switch (step.kind) {
    case "reply":
      queue.push(resolveLine(step.line, requestLine));
      queue.close();
      return;
    case "garbled":
      queue.push("definitely not json");
      queue.close();
      return;
    case "close":
      queue.close();
      return;
    case "oversized":
      queue.fail(ClientError.framing("control line exceeded maximum length"));
      return;
    case "delay":
      void delay(step.ms).then(() => {
        queue.push(resolveLine(step.line, requestLine));
      });
      return;
    case "silent":
      return;
    case "subscription":
      queue.push(resolveLine(step.ack, requestLine));
      for (const event of step.events) {
        queue.push(event);
      }
      queue.close();
      return;
    case "attachSuccess":
    case "attachFailed":
      queue.fail(new Error("attach script step is not supported by the in-memory control channel"));
      return;
  }
}

async function handleSocket(socket: Socket, runtime: Runtime): Promise<void> {
  let buffered: Uint8Array<ArrayBufferLike> = new Uint8Array();
  const chunks = (socket as AsyncIterable<Uint8Array>)[Symbol.asyncIterator]();
  let next = await chunks.next();
  while (next.done !== true) {
    const chunk = next.value;
    buffered = appendBytes(buffered, chunk);
    let newlineIndex = indexOfByte(buffered, LINE_FEED);
    while (newlineIndex >= 0) {
      const lineBytes = buffered.subarray(0, newlineIndex);
      const rawLine = buffered.subarray(0, newlineIndex + 1);
      const remainder = buffered.subarray(newlineIndex + 1);
      const line = decodeLine(lineBytes);
      const step = runtime.nextStep();

      if (step.kind === "attachSuccess" || step.kind === "attachFailed") {
        runtime.pushAttachPrelude(rawLine);
        await applyAttachStep(socket, line, remainder, step, chunks);
        return;
      }

      runtime.pushRequest(line);
      await applyStep(socket, line, step);
      if (step.kind === "close" || step.kind === "silent") {
        return;
      }
      buffered = remainder;
      newlineIndex = indexOfByte(buffered, LINE_FEED);
    }
    next = await chunks.next();
  }

  if (buffered.byteLength > 0) {
    const line = decodeLine(buffered);
    runtime.pushRequest(line);
    await applyStep(socket, line, runtime.nextStep());
  }
}

async function applyStep(socket: Socket, requestLine: string, step: ScriptStep): Promise<void> {
  switch (step.kind) {
    case "reply":
      await writeLine(socket, resolveLine(step.line, requestLine));
      socket.end();
      return;
    case "garbled":
      await writeLine(socket, "definitely not json");
      socket.end();
      return;
    case "close":
      socket.end();
      return;
    case "oversized":
      await writeLine(socket, "a".repeat(MAX_CONTROL_LINE_BYTES + 1));
      socket.end();
      return;
    case "delay":
      await delay(step.ms);
      if (!socket.destroyed) {
        await writeLine(socket, resolveLine(step.line, requestLine));
      }
      return;
    case "silent":
      return;
    case "subscription":
      await writeLine(socket, resolveLine(step.ack, requestLine));
      for (const event of step.events) {
        await writeLine(socket, event);
      }
      socket.end();
      return;
    case "attachSuccess":
    case "attachFailed":
      throw new Error("attach script step reached the framed control handler");
  }
}

async function applyAttachStep(
  socket: Socket,
  preludeLine: string,
  firstRawChunk: Uint8Array,
  step: Extract<ScriptStep, { kind: "attachSuccess" | "attachFailed" }>,
  chunks: AsyncIterator<Uint8Array>,
): Promise<void> {
  switch (step.kind) {
    case "attachFailed":
      await writeLine(socket, resolveLine(step.line, preludeLine));
      socket.end();
      return;
    case "attachSuccess": {
      for (const bytes of step.emit ?? []) {
        await writeBytes(socket, bytes);
      }
      if (firstRawChunk.byteLength > 0) {
        await handleRawChunk(socket, firstRawChunk, step);
      }
      let next = await chunks.next();
      while (next.done !== true) {
        await handleRawChunk(socket, next.value, step);
        next = await chunks.next();
      }
      if (!socket.destroyed) {
        socket.end();
      }
      return;
    }
  }
}

function resolveLine(line: string | ((requestLine: string) => string), requestLine: string): string {
  return typeof line === "function" ? line(requestLine) : line;
}

async function handleRawChunk(
  socket: Socket,
  chunk: Uint8Array,
  step: Extract<ScriptStep, { kind: "attachSuccess" }>,
): Promise<void> {
  if (step.readDelayMs !== undefined) {
    await delay(step.readDelayMs);
  }
  if (step.echo === true) {
    await writeBytes(socket, chunk);
  }
}

function writeLine(socket: Socket, line: string): Promise<void> {
  return writeBytes(socket, new TextEncoder().encode(`${line}\n`));
}

function writeBytes(socket: Socket, bytes: Uint8Array): Promise<void> {
  return new Promise((resolve, reject) => {
    socket.write(bytes, (error) => {
      if (error instanceof Error) {
        reject(error);
        return;
      }
      resolve();
    });
  });
}

function appendBytes(left: Uint8Array, right: Uint8Array): Uint8Array {
  const joined = new Uint8Array(left.byteLength + right.byteLength);
  joined.set(left, 0);
  joined.set(right, left.byteLength);
  return joined;
}

function indexOfByte(bytes: Uint8Array, needle: number): number {
  for (let index = 0; index < bytes.byteLength; index += 1) {
    if (bytes[index] === needle) {
      return index;
    }
  }
  return -1;
}

function decodeLine(bytes: Uint8Array): string {
  const lineBytes = bytes.at(-1) === CARRIAGE_RETURN ? bytes.subarray(0, bytes.byteLength - 1) : bytes;
  return new TextDecoder("utf-8", { fatal: true }).decode(lineBytes);
}

function closeServer(server: Server, sockets: Set<Socket>): Promise<void> {
  for (const socket of sockets) {
    socket.destroy();
  }
  return new Promise((resolve) => {
    server.close(() => {
      resolve();
    });
  });
}

async function listen(server: Server, options: string | { host: string; port: number }): Promise<Error | null> {
  const error = new Promise<Error>((resolve) => {
    server.once("error", (caught) => {
      resolve(caught);
    });
  });
  const listening = new Promise<null>((resolve) => {
    server.once("listening", () => {
      resolve(null);
    });
  });

  try {
    server.listen(options);
  } catch (caught: unknown) {
    return caught instanceof Error ? caught : new Error(String(caught));
  }

  return Promise.race([error, listening]);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function isAddressInfo(address: string | AddressInfo | null): address is AddressInfo {
  return typeof address === "object" && address !== null && "port" in address;
}

function isPermissionDenied(error: Error): boolean {
  const code = (error as { code?: unknown }).code;
  return code === "EPERM" || error.message.includes("Failed to listen");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
