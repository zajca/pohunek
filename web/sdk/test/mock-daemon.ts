import { rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { MAX_CONTROL_LINE_BYTES, type SessionInfo } from "@pohunek/protocol";
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
  | { kind: "subscription"; ack: string | ((requestLine: string) => string); events: string[] };

export interface MockDaemon {
  endpoint: MockEndpoint;
  nextRequest(): Promise<string>;
  expectNoRequest(timeoutMs: number): Promise<void>;
  close(): Promise<void>;
}

let nextSocketId = 0;

export async function startUnixDaemon(steps: ScriptStep[]): Promise<MockDaemon> {
  const socketPath = join(
    tmpdir(),
    `pohunek-sdk-${process.pid}-${nextSocketId++}.sock`,
  );
  rmSync(socketPath, { force: true });
  const runtime = createRuntime(steps);
  const server = createServer((socket) => {
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
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {
      await closeServer(server, runtime.sockets);
      rmSync(socketPath, { force: true });
    },
  };
}

export async function startTcpDaemon(steps: ScriptStep[]): Promise<MockDaemon> {
  const runtime = createRuntime(steps);
  const server = createServer((socket) => {
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
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {
      await closeServer(server, runtime.sockets);
    },
  };
}

export function okResponseLine(id: string, ok: unknown): string {
  return JSON.stringify({ v: 1, id, ok });
}

export function errResponseLine(id: string, err: unknown): string {
  return JSON.stringify({ v: 1, id, err });
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
    expectNoRequest: (timeoutMs: number): Promise<void> => runtime.expectNoRequest(timeoutMs),
    close: async (): Promise<void> => {},
  };
}

export function minimalSessionInfo(): SessionInfo {
  return {
    id: "s-test-1",
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
  nextRequest(): Promise<string>;
  expectNoRequest(timeoutMs: number): Promise<void>;
}

function createRuntime(steps: ScriptStep[]): Runtime {
  const sockets = new Set<Socket>();
  const requests: string[] = [];
  const waiters: Array<(line: string) => void> = [];
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
    nextRequest(): Promise<string> {
      const line = requests.shift();
      if (line !== undefined) {
        return Promise.resolve(line);
      }
      return new Promise((resolve) => {
        waiters.push(resolve);
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
    const readable = new ReadableStream<Uint8Array>({
      start(controller): void {
        controller.close();
      },
    });
    const writable = new WritableStream<Uint8Array>();
    return Promise.resolve({
      readable,
      writable,
      close: (): Promise<void> => Promise.resolve(),
    });
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
  }
}

async function handleSocket(socket: Socket, runtime: Runtime): Promise<void> {
  for await (const line of readLines(socket)) {
    runtime.pushRequest(line);
    const step = runtime.nextStep();
    await applyStep(socket, line, step);
    if (step.kind === "close" || step.kind === "silent") {
      return;
    }
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
  }
}

async function* readLines(socket: Socket): AsyncGenerator<string> {
  const decoder = new TextDecoder();
  let buffered = "";
  for await (const chunk of socket as AsyncIterable<Uint8Array>) {
    buffered += decoder.decode(chunk, { stream: true });
    let newlineIndex = buffered.indexOf("\n");
    while (newlineIndex >= 0) {
      const line = buffered.slice(0, newlineIndex).replace(/\r$/, "");
      buffered = buffered.slice(newlineIndex + 1);
      yield line;
      newlineIndex = buffered.indexOf("\n");
    }
  }
  buffered += decoder.decode();
  if (buffered.length > 0) {
    yield buffered.replace(/\r$/, "");
  }
}

function resolveLine(line: string | ((requestLine: string) => string), requestLine: string): string {
  return typeof line === "function" ? line(requestLine) : line;
}

function writeLine(socket: Socket, line: string): Promise<void> {
  return new Promise((resolve, reject) => {
    socket.write(`${line}\n`, (error) => {
      if (error instanceof Error) {
        reject(error);
        return;
      }
      resolve();
    });
  });
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
