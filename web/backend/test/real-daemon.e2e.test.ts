import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "bun:test";
import {
  startBackendFromEnv,
  type BackendHandle,
  type BackendHostEntry,
  type BackendLogger,
} from "@pohunek/backend";
import {
  Client,
  PROTOCOL_VERSION,
  attachRawWs,
  type CatchAllEvent,
  type ProtocolEvent,
  type RawStream,
  type Request,
  type Subscription,
} from "@pohunek/sdk/browser";
import { startDurableWorkerFixture } from "@pohunek/testkit";

const E2E_ENABLED = process.env["POHUNEK_E2E"] === "1";

const TEST_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(TEST_DIR, "../../..");
const DEFAULT_DAEMON_BIN = join(REPO_ROOT, "target", "debug", "pohunekd");
const APP_DIR = "pohunek";
const SOCKET_NAME = "daemon.sock";
const LOCAL_HOST = "local";
const LOOPBACK_HOST = "127.0.0.1";
const SESSION_COLS = 80;
const SESSION_ROWS = 24;
const DAEMON_READY_TIMEOUT_MS = 10_000;
const DAEMON_READY_POLL_MS = 50;
const DAEMON_CONNECT_TIMEOUT_MS = 750;
const DAEMON_REQUEST_TIMEOUT_MS = 3_000;
const EVENT_TIMEOUT_MS = 10_000;
const ATTACH_TIMEOUT_MS = 10_000;
const READ_MARKER_TIMEOUT_MS = 10_000;
const DAEMON_TERM_GRACE_MS = 5_000;
const E2E_TEST_TIMEOUT_MS = 60_000;
const BACKEND_DISCOVER_INTERVAL_SECONDS = "60";
const MARKER = "pohunek-backend-real-daemon-e2e-marker";
const SAFE_AGENT_PREFERENCE = ["shell"] as const;
const NETBIRD_FIXTURE_SCRIPT = `#!/bin/sh
if [ "$1" != "status" ] || [ "$2" != "--json" ]; then
  exit 2
fi
printf '%s\\n' '{"daemonStatus":"Connected","peers":{"details":[]}}'
`;
const encoder = new TextEncoder();

const silentLogger: BackendLogger = {
  log(): void {},
};

interface DaemonHarness {
  readonly socketPath: string;
  readonly tempRoot: string;
  stdout(): string;
  stderr(): string;
  stop(): Promise<void>;
}

interface ExitStatus {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
}

type SkippableTest = typeof test & { readonly skip: typeof test };

const skippableTest = test as SkippableTest;
const realDaemonTest = E2E_ENABLED ? skippableTest : skippableTest.skip;

realDaemonTest("real pohunekd supports the browser lifecycle through the backend origin", async () => {
  await withTimeout(
    withDaemon(async (daemon) => {
      await withBackend(daemon, async (backend) => {
        await runBrowserScenario(daemon, backend);
      });
    }),
    E2E_TEST_TIMEOUT_MS,
    `backend real-daemon e2e did not finish within ${E2E_TEST_TIMEOUT_MS}ms`,
  );
});

async function runBrowserScenario(
  daemon: DaemonHarness,
  backend: BackendHandle,
): Promise<void> {
  let client: Client | undefined;
  let eventClient: Client | undefined;
  let raw: RawStream | undefined;
  let sessionId: string | undefined;
  let streamId: string | undefined;
  let stopped = false;

  try {
    const hostsResponse = await fetch(`${backend.url}/api/hosts`);
    expect(hostsResponse.status).toBe(200);
    expect(hostsResponse.headers.get("content-type")).toBe("application/json; charset=utf-8");
    const hosts = await readHosts(hostsResponse);
    expect(hosts.length).toBe(1);
    const local = hosts[0];
    expect(local?.host).toBe(LOCAL_HOST);
    expect(local?.reachability).toBe("reachable_daemon");
    expect(local?.protocol_version).toBe(PROTOCOL_VERSION);

    client = await Client.connectWs(backend.url, LOCAL_HOST, connectOptions());
    const health = await client.call("daemon.health", null);
    expect(health.protocol_version).toBe(PROTOCOL_VERSION);
    expect(local?.daemon_version).toBe(health.daemon_version);

    const capabilities = await client.call("host.inspect", null);
    expect(capabilities.protocol_version).toBe(PROTOCOL_VERSION);
    const agent = selectSafeAgent(capabilities.supported_agents, capabilities.runtimes);

    eventClient = await Client.connectWs(backend.url, LOCAL_HOST, connectOptions());
    const subscription = await eventClient.subscribe(subscribeRequest("backend-real-daemon-subscribe"));

    const created = await client.call("session.new", {
      agent,
      cols: SESSION_COLS,
      rows: SESSION_ROWS,
    });
    sessionId = created.id;
    expect(sessionId.length > 0).toBe(true);

    const createdEvent = await waitForEvent(
      subscription,
      (event): event is Extract<ProtocolEvent, { event: "session_created" }> =>
        isSessionEventFor(event, "session_created", sessionId),
      `session_created for ${sessionId}`,
    );
    expect(createdEvent.session.id).toBe(sessionId);

    const attached = await client.call("session.attach", { session_id: sessionId });
    streamId = attached.stream_id;
    expect(streamId.length > 0).toBe(true);
    raw = await withTimeout(
      attachRawWs(backend.url, LOCAL_HOST, streamId, connectOptions()),
      ATTACH_TIMEOUT_MS,
      `backend attach stream did not open within ${ATTACH_TIMEOUT_MS}ms`,
    );

    await writeShellMarker(raw);
    const output = await readUntilContains(raw, MARKER);
    expect(output.includes(MARKER)).toBe(true);

    const detached = await client.call("session.detach", { stream_id: streamId });
    expect(detached.detached).toBe(true);
    streamId = undefined;
    await raw.close();
    raw = undefined;

    const stoppedResult = await client.call("session.stop", sessionId);
    expect(stoppedResult.stopped).toBe(true);
    stopped = true;

    const stoppedEvent = await waitForEvent(
      subscription,
      (event): event is Extract<ProtocolEvent, { event: "session_stopped" }> =>
        isSessionEventFor(event, "session_stopped", sessionId),
      `session_stopped for ${sessionId}`,
    );
    expect(stoppedEvent.session.id).toBe(sessionId);
  } catch (error: unknown) {
    throw addDaemonContext(error, daemon);
  } finally {
    if (streamId !== undefined && client !== undefined) {
      await client.call("session.detach", { stream_id: streamId }).catch(() => undefined);
    }
    await raw?.close().catch(() => undefined);
    if (!stopped && sessionId !== undefined && client !== undefined) {
      await client.call("session.stop", sessionId).catch(() => undefined);
    }
    await client?.close().catch(() => undefined);
    await eventClient?.close().catch(() => undefined);
  }
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

async function withBackend<T>(
  daemon: DaemonHarness,
  run: (backend: BackendHandle) => Promise<T>,
): Promise<T> {
  const backend = await startBackendFromEnv(
    {
      POHUNEK_BACKEND_BIND_HOST: LOOPBACK_HOST,
      POHUNEK_BACKEND_PORT: "0",
      POHUNEK_BACKEND_ALLOW_LOOPBACK: "1",
      POHUNEK_BACKEND_DAEMON_SOCKET: daemon.socketPath,
      POHUNEK_BACKEND_DISCOVER_INTERVAL: BACKEND_DISCOVER_INTERVAL_SECONDS,
      POHUNEK_BACKEND_STATIC_DIR: daemon.tempRoot,
    },
    silentLogger,
  );
  let result: T | undefined;
  let failure: unknown;

  try {
    result = await run(backend);
  } catch (error: unknown) {
    failure = error;
  }

  try {
    await backend.close();
  } catch (error: unknown) {
    if (failure !== undefined) {
      throw new AggregateError([failure, error], "e2e scenario and backend teardown both failed");
    }
    throw error;
  }

  if (failure !== undefined) {
    throw errorFromUnknown(failure);
  }
  return result as T;
}

async function startDaemon(): Promise<DaemonHarness> {
  const tempRoot = await mkdtemp(join(tmpdir(), "pohunek-backend-e2e-"));
  const dirs = {
    runtime: join(tempRoot, "runtime"),
    data: join(tempRoot, "data"),
    state: join(tempRoot, "state"),
    cache: join(tempRoot, "cache"),
    config: join(tempRoot, "config"),
    home: join(tempRoot, "home"),
    bin: join(tempRoot, "bin"),
  };
  await Promise.all(Object.values(dirs).map((dir) => mkdir(dir, { recursive: true })));
  await writeFile(join(dirs.bin, "netbird"), NETBIRD_FIXTURE_SCRIPT, { mode: 0o700 });

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
      // Keep host discovery local-only and runtime probing deterministic. The
      // shell agent still uses the explicit absolute path below.
      PATH: dirs.bin,
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
    await waitForDaemonSocket(socketPath, () => exitStatus, () => spawnError, logs);
  } catch (error: unknown) {
    await stopChild(child, exitPromise, () => exitStatus).catch(() => undefined);
    await worker.stop();
    await rm(tempRoot, { recursive: true, force: true });
    throw error;
  }

  return {
    socketPath,
    tempRoot,
    ...logs(),
    stop: async (): Promise<void> => {
      const status = await stopChild(child, exitPromise, () => exitStatus);
      await worker.stop();
      await rm(tempRoot, { recursive: true, force: true });
      if (status.code !== 0 || status.signal !== null) {
        throw new Error(
          `pohunekd exited uncleanly (code=${String(status.code)}, signal=${String(status.signal)})\n`
            + `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
        );
      }
    },
  };
}

async function waitForDaemonSocket(
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
        `pohunekd exited before readiness (code=${String(status.code)}, signal=${String(status.signal)})\n`
          + `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
      );
    }

    try {
      await access(socketPath);
      return;
    } catch (error: unknown) {
      lastError = error;
      await delay(DAEMON_READY_POLL_MS);
    }
  }

  throw new Error(
    `pohunekd did not expose its socket within ${DAEMON_READY_TIMEOUT_MS}ms\n`
      + `socket: ${socketPath}\nlast error: ${formatUnknown(lastError)}\n`
      + `stdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
  );
}

async function stopChild(
  child: ChildProcess,
  exitPromise: Promise<ExitStatus>,
  exitStatus: () => ExitStatus | undefined,
): Promise<ExitStatus> {
  const current = exitStatus();
  if (current !== undefined) {
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

function selectSafeAgent(
  supportedAgents: readonly string[],
  runtimes: readonly { readonly agent: string; readonly available: boolean }[],
): string {
  const supported = new Set(supportedAgents);
  const available = new Set(
    runtimes.filter((runtime) => runtime.available).map((runtime) => runtime.agent),
  );
  for (const candidate of SAFE_AGENT_PREFERENCE) {
    if (supported.has(candidate) && available.has(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    `host.inspect reported no safe available agent; supported=${supportedAgents.join(",")}; `
      + `available=${Array.from(available).join(",")}`,
  );
}

function subscribeRequest(id: string): Request {
  return {
    v: PROTOCOL_VERSION,
    id,
    method: "subscribe",
    params: null,
  };
}

async function writeShellMarker(raw: RawStream): Promise<void> {
  const writer = raw.writable.getWriter();
  try {
    await writer.write(
      encoder.encode("printf 'pohunek-backend-real-%s\\n' 'daemon-e2e-marker'\n"),
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

  throw new Error(
    `raw attach output did not contain ${marker} within ${READ_MARKER_TIMEOUT_MS}ms; output:\n${output}`,
  );
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
      throw new Error(
        `subscription closed while waiting for ${description}; seen events: ${seen.join(", ")}`,
      );
    }
    seen.push(event.event);
    if (predicate(event)) {
      return event;
    }
  }

  throw new Error(`timed out waiting for ${description}; seen events: ${seen.join(", ")}`);
}

function isSessionEventFor(
  event: ProtocolEvent | CatchAllEvent,
  eventName: "session_created" | "session_stopped",
  sessionId: string | undefined,
): event is Extract<ProtocolEvent, { event: "session_created" | "session_stopped" }> {
  if (sessionId === undefined || event.event !== eventName) {
    return false;
  }
  const session = event["session"];
  return isRecord(session) && session["id"] === sessionId;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function readHosts(response: Response): Promise<readonly BackendHostEntry[]> {
  return response.json() as Promise<readonly BackendHostEntry[]>;
}

function connectOptions(): { connectTimeoutMs: number; requestTimeoutMs: number } {
  return {
    connectTimeoutMs: DAEMON_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: DAEMON_REQUEST_TIMEOUT_MS,
  };
}

function addDaemonContext(error: unknown, daemon: DaemonHarness): Error {
  const message = error instanceof Error ? error.message : String(error);
  const wrapped = new Error(
    `${message}\n`
      + `daemon temp root: ${daemon.tempRoot}\n`
      + `daemon socket: ${daemon.socketPath}\n`
      + `daemon stdout:\n${daemon.stdout()}\n`
      + `daemon stderr:\n${daemon.stderr()}`,
  );
  if (error instanceof Error && error.stack !== undefined) {
    wrapped.stack = error.stack;
  }
  return wrapped;
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

function errorFromUnknown(error: unknown): Error {
  if (error instanceof Error) {
    return error;
  }
  return new Error(String(error));
}
