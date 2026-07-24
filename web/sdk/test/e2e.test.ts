import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { access, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "bun:test";
import { PROTOCOL_VERSION, type ProtocolEvent } from "@pohunek/protocol";
import { startRelay, type RelayHandle } from "@pohunek/backend";
import { startDurableWorkerFixture } from "@pohunek/testkit";
import {
  Client,
  attachRawLocal,
  attachRawWs,
  connectLocal,
  type CatchAllEvent,
  type RawStream,
  type Request,
  type Subscription,
} from "@pohunek/sdk";

const E2E_ENABLED = process.env["POHUNEK_E2E"] === "1";

const TEST_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(TEST_DIR, "../../..");
const DEFAULT_DAEMON_BIN = join(REPO_ROOT, "target", "debug", "pohunekd");
const APP_DIR = "pohunek";
const SOCKET_NAME = "daemon.sock";
const E2E_HOST = "local-e2e";
const SESSION_COLS = 80;
const SESSION_ROWS = 24;
const DAEMON_READY_TIMEOUT_MS = 10_000;
const DAEMON_READY_POLL_MS = 50;
const DAEMON_CONNECT_TIMEOUT_MS = 750;
const DAEMON_REQUEST_TIMEOUT_MS = 1_500;
const EVENT_TIMEOUT_MS = 10_000;
const ATTACH_TIMEOUT_MS = 10_000;
const READ_MARKER_TIMEOUT_MS = 10_000;
const DAEMON_TERM_GRACE_MS = 5_000;
const E2E_TEST_TIMEOUT_MS = 60_000;
const MARKER = "pohunek-e2e-marker";
const encoder = new TextEncoder();

interface DaemonHarness {
  readonly socketPath: string;
  readonly tempRoot: string;
  stdout(): string;
  stderr(): string;
  stop(): Promise<void>;
}

interface E2eTransport {
  readonly name: string;
  connectClient(): Promise<Client>;
  attachRaw(streamId: string): Promise<RawStream>;
  close(): Promise<void>;
}

interface ExitStatus {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
}

e2eTest(
  "real pohunekd supports the socket transport session lifecycle and attach round-trip",
  async () => {
    await withDaemon(async (daemon) => {
      await runSessionScenario(daemon, socketTransport(daemon));
    });
  },
  E2E_TEST_TIMEOUT_MS,
);

e2eTest(
  "real pohunekd supports the WebSocket relay transport session lifecycle and attach round-trip",
  async () => {
    await withDaemon(async (daemon) => {
      const transport = await wsTransport(daemon);
      await runSessionScenario(daemon, transport);
    });
  },
  E2E_TEST_TIMEOUT_MS,
);

async function runSessionScenario(daemon: DaemonHarness, transport: E2eTransport): Promise<void> {
  let client: Client | undefined;
  let eventClient: Client | undefined;
  let raw: RawStream | undefined;
  let sessionId: string | undefined;
  let streamId: string | undefined;
  let stopped = false;

  try {
    client = await transport.connectClient();
    const version = await client.handshake();
    expect(version).toBe(PROTOCOL_VERSION);

    eventClient = await transport.connectClient();
    const subscription = await eventClient.subscribe(subscribeRequest(`${transport.name}-subscribe`));

    const created = await client.call("session.new", {
      agent: "shell",
      cols: SESSION_COLS,
      rows: SESSION_ROWS,
    });
    sessionId = created.id;
    expect(created.id.length).toBeGreaterThan(0);

    const listed = await client.call("session.list", {});
    expect(listed.some((session) => session.id === sessionId)).toBe(true);

    const createdEvent = await waitForEvent(
      subscription,
      (event): event is Extract<ProtocolEvent, { event: "session_created" }> =>
        isSessionCreatedFor(event, sessionId),
      `session_created for ${sessionId}`,
    );
    expect(createdEvent.session.id).toBe(sessionId);

    const attached = await client.call("session.attach", { session_id: sessionId });
    streamId = attached.stream_id;
    expect(streamId.length).toBeGreaterThan(0);

    raw = await withTimeout(
      transport.attachRaw(streamId),
      ATTACH_TIMEOUT_MS,
      `${transport.name} attach raw stream did not open within ${ATTACH_TIMEOUT_MS}ms`,
    );
    await writeShellCommand(raw);
    const output = await readUntilContains(raw, MARKER);
    expect(output).toContain(MARKER);

    const agentStateEvent = await waitForEvent(
      subscription,
      (event): event is Extract<ProtocolEvent, { event: "agent_state" }> =>
        isAgentStateFor(event, sessionId),
      `agent_state for ${sessionId}`,
    );
    expect(agentStateEvent.session_id).toBe(sessionId);

    const detached = await client.call("session.detach", { stream_id: streamId });
    expect(detached.detached).toBe(true);
    streamId = undefined;

    const stoppedResult = await client.call("session.stop", sessionId);
    expect(stoppedResult.stopped).toBe(true);
    stopped = true;
  } catch (error: unknown) {
    throw addDaemonContext(error, daemon);
  } finally {
    if (streamId !== undefined && client !== undefined) {
      await client.call("session.detach", { stream_id: streamId }).catch(() => undefined);
    }
    if (!stopped && sessionId !== undefined && client !== undefined) {
      await client.call("session.stop", sessionId).catch(() => undefined);
    }
    await raw?.close().catch(() => undefined);
    await client?.close().catch(() => undefined);
    await eventClient?.close().catch(() => undefined);
    await transport.close();
  }
}

function socketTransport(daemon: DaemonHarness): E2eTransport {
  return {
    name: "socket",
    connectClient: (): Promise<Client> => connectLocal(daemon.socketPath, connectOptions()),
    attachRaw: (streamId: string): Promise<RawStream> =>
      attachRawLocal(daemon.socketPath, streamId, connectOptions()),
    close: (): Promise<void> => Promise.resolve(),
  };
}

async function wsTransport(daemon: DaemonHarness): Promise<E2eTransport> {
  const relay = await startRelay({
    bindHost: "127.0.0.1",
    port: 0,
    allowLoopbackBind: true,
    targets: new Map([[E2E_HOST, { kind: "unix", socketPath: daemon.socketPath }]]),
  });
  return {
    name: "websocket",
    connectClient: (): Promise<Client> => Client.connectWs(relay.url, E2E_HOST, connectOptions()),
    attachRaw: (streamId: string): Promise<RawStream> =>
      attachRawWs(relay.url, E2E_HOST, streamId, connectOptions()),
    close: (): Promise<void> => closeRelay(relay),
  };
}

async function withDaemon<T>(run: (daemon: DaemonHarness) => Promise<T>): Promise<T> {
  const daemon = await startDaemon();
  let result: T | undefined;
  let failure: unknown;

  try {
    result = await run(daemon);
  } catch (error: unknown) {
    failure = error;
  }

  try {
    await daemon.stop();
  } catch (error: unknown) {
    if (failure !== undefined) {
      throw new AggregateError([failure, error], "e2e scenario and daemon teardown both failed");
    }
    throw error;
  }

  if (failure !== undefined) {
    throw errorFromUnknown(failure);
  }
  return result as T;
}

async function startDaemon(): Promise<DaemonHarness> {
  const tempRoot = await mkdtemp(join(tmpdir(), "pohunek-sdk-e2e-"));
  const dirs = {
    runtime: join(tempRoot, "runtime"),
    data: join(tempRoot, "data"),
    state: join(tempRoot, "state"),
    cache: join(tempRoot, "cache"),
    config: join(tempRoot, "config"),
    home: join(tempRoot, "home"),
  };
  await Promise.all(Object.values(dirs).map((dir) => mkdir(dir, { recursive: true })));

  const daemonBin = daemonBinaryPath();
  const worker = await startDurableWorkerFixture({
    daemonBin,
    runtimeDir: dirs.runtime,
    dataDir: dirs.data,
    stateDir: dirs.state,
    configDir: dirs.config,
    sessionId: "s-1",
  });
  const stdoutChunks: string[] = [];
  const stderrChunks: string[] = [];
  let exitStatus: ExitStatus | undefined;
  let spawnError: Error | undefined;

  const child = spawn(daemonBin, [], {
    cwd: tempRoot,
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: dirs.runtime,
      XDG_DATA_HOME: dirs.data,
      XDG_STATE_HOME: dirs.state,
      XDG_CACHE_HOME: dirs.cache,
      XDG_CONFIG_HOME: dirs.config,
      HOME: dirs.home,
      // Force a plain, deterministic shell for the `shell` agent. The daemon
      // uses `$SHELL` (falling back to `/bin/sh`); inheriting the developer's
      // interactive shell (e.g. zsh) triggers first-run wizards under the
      // isolated empty HOME, which swallow the probe command and make the
      // attach round-trip non-deterministic.
      SHELL: "/bin/sh",
      POHUNEK_WORKER_UNIT_TEMPLATE: worker.unitTemplate,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk: string): void => {
    stdoutChunks.push(chunk);
  });
  child.stderr.on("data", (chunk: string): void => {
    stderrChunks.push(chunk);
  });
  child.once("error", (error: Error): void => {
    spawnError = error;
  });
  const exitPromise = once(child, "exit").then(([code, signal]) => {
    exitStatus = { code: code as number | null, signal: signal as NodeJS.Signals | null };
    return exitStatus;
  });

  const socketPath = join(dirs.runtime, APP_DIR, SOCKET_NAME);
  const logs = (): Pick<DaemonHarness, "stdout" | "stderr"> => ({
    stdout: () => stdoutChunks.join(""),
    stderr: () => stderrChunks.join(""),
  });

  try {
    await waitForDaemonReady(socketPath, () => exitStatus, () => spawnError, logs);
  } catch (error: unknown) {
    await stopChild(child, exitPromise, () => exitStatus, { allowAlreadyExited: true }).catch(() => undefined);
    await worker.stop();
    await rm(tempRoot, { recursive: true, force: true });
    throw error;
  }

  return {
    socketPath,
    tempRoot,
    ...logs(),
    stop: async (): Promise<void> => {
      const status = await stopChild(child, exitPromise, () => exitStatus, { allowAlreadyExited: false });
      await worker.stop();
      await rm(tempRoot, { recursive: true, force: true });
      if (status.code !== 0 || status.signal !== null) {
        throw new Error(
          `pohunekd exited uncleanly (code=${String(status.code)}, signal=${String(status.signal)})\n` +
            `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
        );
      }
    },
  };
}

async function waitForDaemonReady(
  socketPath: string,
  exitStatus: () => ExitStatus | undefined,
  spawnError: () => Error | undefined,
  logs: () => Pick<DaemonHarness, "stdout" | "stderr">,
): Promise<void> {
  const deadline = Date.now() + DAEMON_READY_TIMEOUT_MS;
  let lastError: unknown;

  while (Date.now() < deadline) {
    const currentSpawnError = spawnError();
    if (currentSpawnError !== undefined) {
      throw currentSpawnError;
    }
    const status = exitStatus();
    if (status !== undefined) {
      throw new Error(
        `pohunekd exited before readiness (code=${String(status.code)}, signal=${String(status.signal)})\n` +
          `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
      );
    }

    try {
      await access(socketPath);
      await verifyHandshake(socketPath);
      return;
    } catch (error: unknown) {
      lastError = error;
      await delay(DAEMON_READY_POLL_MS);
    }
  }

  throw new Error(
    `pohunekd did not become ready within ${DAEMON_READY_TIMEOUT_MS}ms\n` +
      `socket: ${socketPath}\nlast error: ${formatUnknown(lastError)}\n` +
      `stdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
  );
}

async function verifyHandshake(socketPath: string): Promise<void> {
  let client: Client | undefined;
  try {
    client = await connectLocal(socketPath, connectOptions());
    const version = await client.handshake();
    if (version !== PROTOCOL_VERSION) {
      throw new Error(`expected protocol version ${PROTOCOL_VERSION}, got ${version}`);
    }
  } finally {
    await client?.close().catch(() => undefined);
  }
}

async function stopChild(
  child: ChildProcess,
  exitPromise: Promise<ExitStatus>,
  exitStatus: () => ExitStatus | undefined,
  options: { readonly allowAlreadyExited: boolean },
): Promise<ExitStatus> {
  const current = exitStatus();
  if (current !== undefined) {
    if (options.allowAlreadyExited) {
      return current;
    }
    return current;
  }

  child.kill("SIGTERM");
  try {
    return await withTimeout(
      exitPromise,
      DAEMON_TERM_GRACE_MS,
      `pohunekd did not exit within ${DAEMON_TERM_GRACE_MS}ms after SIGTERM`,
    );
  } catch (error: unknown) {
    child.kill("SIGKILL");
    await exitPromise;
    throw error;
  }
}

function daemonBinaryPath(): string {
  const override = process.env["POHUNEK_DAEMON_BIN"];
  if (override !== undefined && override.length > 0) {
    if (!isAbsolute(override)) {
      throw new Error("POHUNEK_DAEMON_BIN must be an absolute path");
    }
    return override;
  }
  return DEFAULT_DAEMON_BIN;
}

function subscribeRequest(id: string): Request {
  return {
    v: PROTOCOL_VERSION,
    id,
    method: "subscribe",
    params: null,
  };
}

async function writeShellCommand(raw: RawStream): Promise<void> {
  const writer = raw.writable.getWriter();
  try {
    await writer.write(
      encoder.encode("printf '\\033]0;working\\007'; printf 'pohunek-e2e-%s\\n' 'marker'\n"),
    );
  } finally {
    writer.releaseLock();
  }
}

async function readUntilContains(raw: RawStream, marker: string): Promise<string> {
  const reader = raw.readable.getReader();
  const decoder = new TextDecoder();
  let output = "";
  const deadline = Date.now() + READ_MARKER_TIMEOUT_MS;

  try {
    while (Date.now() < deadline) {
      const remainingMs = deadline - Date.now();
      const chunk = await withTimeout(
        reader.read(),
        remainingMs,
        `raw attach output did not contain ${marker} within ${READ_MARKER_TIMEOUT_MS}ms`,
      );
      if (chunk.done === true) {
        throw new Error(`raw attach stream closed before output contained ${marker}; output:\n${output}`);
      }
      output += decoder.decode(chunk.value, { stream: true });
      if (output.includes(marker)) {
        return output;
      }
    }
  } finally {
    reader.releaseLock();
  }

  throw new Error(`raw attach output did not contain ${marker} within ${READ_MARKER_TIMEOUT_MS}ms; output:\n${output}`);
}

async function waitForEvent<T extends ProtocolEvent>(
  subscription: Subscription,
  predicate: (event: ProtocolEvent | CatchAllEvent) => event is T,
  description: string,
): Promise<T> {
  const seen: string[] = [];
  const deadline = Date.now() + EVENT_TIMEOUT_MS;

  while (Date.now() < deadline) {
    const event = await withTimeout(
      subscription.nextEvent(),
      deadline - Date.now(),
      `timed out waiting for ${description}; seen events: ${seen.join(", ")}`,
    );
    if (event === null) {
      throw new Error(`subscription closed while waiting for ${description}; seen events: ${seen.join(", ")}`);
    }
    seen.push(event.event);
    if (predicate(event)) {
      return event;
    }
  }

  throw new Error(`timed out waiting for ${description}; seen events: ${seen.join(", ")}`);
}

function connectOptions(): { connectTimeoutMs: number; requestTimeoutMs: number } {
  return {
    connectTimeoutMs: DAEMON_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: DAEMON_REQUEST_TIMEOUT_MS,
  };
}

async function closeRelay(relay: RelayHandle): Promise<void> {
  await relay.close();
}

function addDaemonContext(error: unknown, daemon: DaemonHarness): Error {
  const message = error instanceof Error ? error.message : String(error);
  const wrapped = new Error(
    `${message}\n` +
      `daemon temp root: ${daemon.tempRoot}\n` +
      `daemon socket: ${daemon.socketPath}\n` +
      `daemon stdout:\n${daemon.stdout()}\n` +
      `daemon stderr:\n${daemon.stderr()}`,
  );
  if (error instanceof Error && error.stack !== undefined) {
    wrapped.stack = error.stack;
  }
  return wrapped;
}

function isSessionCreatedFor(
  event: ProtocolEvent | CatchAllEvent,
  sessionId: string | undefined,
): event is Extract<ProtocolEvent, { event: "session_created" }> {
  if (sessionId === undefined || event.event !== "session_created") {
    return false;
  }
  const session = event["session"];
  return isRecord(session) && session["id"] === sessionId;
}

function isAgentStateFor(
  event: ProtocolEvent | CatchAllEvent,
  sessionId: string | undefined,
): event is Extract<ProtocolEvent, { event: "agent_state" }> {
  if (sessionId === undefined || event.event !== "agent_state") {
    return false;
  }
  return event["session_id"] === sessionId;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, message: string): Promise<T> {
  return new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(message));
    }, timeoutMs);

    promise.then(
      (value) => {
        clearTimeout(timer);
        resolvePromise(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(errorFromUnknown(error));
      },
    );
  });
}

function delay(ms: number): Promise<void> {
  return new Promise((resolvePromise) => {
    setTimeout(resolvePromise, ms);
  });
}

function formatUnknown(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function e2eTest(name: string, fn: () => void | Promise<void>, timeoutMs: number): void {
  if (E2E_ENABLED) {
    test(name, fn, timeoutMs);
    return;
  }
  test.skip(name, fn, timeoutMs);
}

function errorFromUnknown(error: unknown): Error {
  if (error instanceof Error) {
    return error;
  }
  return new Error(String(error));
}
