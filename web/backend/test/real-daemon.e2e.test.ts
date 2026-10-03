import { constants } from "node:fs";
import { access, lstat, mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { expect, test } from "bun:test";
import {
  loadBackendConfig,
  startBackendFromEnv,
  type BackendHandle,
  type BackendHostEntry,
  type BackendLogger,
} from "@pohunek/backend";
import { MACOS_DEFAULT_RUNTIME_PREFIX } from "@pohunek/sdk";
import {
  Client,
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  attachRawWs,
  type CatchAllEvent,
  type ProtocolEvent,
  type RawStream,
  type Request,
  type Subscription,
} from "@pohunek/sdk/browser";
import {
  GIT_FIXTURE_SCRIPT,
  addDaemonContext,
  createFixtureRoot,
  daemonBinaryPath,
  isRecord,
  startDaemonProcess,
  startDurableWorkerFixture,
  withResource,
  withTimeout,
  type DaemonContext,
} from "@pohunek/testkit";

const E2E_ENABLED = process.env["POHUNEK_E2E"] === "1";

const APP_DIR = "pohunek";
const SOCKET_NAME = "daemon.sock";
const LOCAL_HOST = "local";
const LOOPBACK_HOST = "127.0.0.1";
const SESSION_COLS = 80;
const SESSION_ROWS = 24;
const DAEMON_CONNECT_TIMEOUT_MS = 750;
const DAEMON_REQUEST_TIMEOUT_MS = 3_000;
const EVENT_TIMEOUT_MS = 10_000;
const ATTACH_TIMEOUT_MS = 10_000;
const READ_MARKER_TIMEOUT_MS = 10_000;
const E2E_TEST_TIMEOUT_MS = 60_000;
// Bun remains a backstop so the scenario-specific timeout reports the useful diagnostic first.
const BUN_TEST_TIMEOUT_BACKSTOP_MARGIN_MS = 5_000;
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

interface DaemonHarness extends DaemonContext {
  readonly env: NodeJS.ProcessEnv;
  stop(): Promise<void>;
}

interface SkippableTest {
  (name: string, fn: () => void | Promise<void>, timeout?: number): void;
  readonly skip: SkippableTest;
}

const skippableTest = test as SkippableTest;
const realDaemonTest: SkippableTest = E2E_ENABLED ? skippableTest : skippableTest.skip;
const DEFAULT_RUNTIME_E2E_ENABLED = process.platform === "darwin"
  && process.env["POHUNEK_E2E_DEFAULT_RUNTIME"] === "1";
const macosDefaultRuntimeTest: SkippableTest = E2E_ENABLED && DEFAULT_RUNTIME_E2E_ENABLED
  ? skippableTest
  : skippableTest.skip;

realDaemonTest(
  "real pohunekd supports the browser lifecycle through the backend origin",
  async () => {
    await withTimeout(
      withDaemon(async (daemon) => {
        await withBackend(daemon, async (backend) => {
          await runBrowserScenario(daemon, backend);
        });
      }),
      E2E_TEST_TIMEOUT_MS,
      `backend real-daemon e2e did not finish within ${E2E_TEST_TIMEOUT_MS}ms`,
    );
  },
  E2E_TEST_TIMEOUT_MS + BUN_TEST_TIMEOUT_BACKSTOP_MARGIN_MS,
);

macosDefaultRuntimeTest(
  "backend reaches a real pohunekd that uses the macOS default runtime directory",
  async () => {
    await withTimeout(
      withDaemon(async (daemon) => {
        expect(daemon.env["XDG_RUNTIME_DIR"]).toBeUndefined();
        expect(daemon.socketPath).toBe(`/private/tmp/pohunek-${String(process.geteuid?.())}/daemon.sock`);
        await withBackend(daemon, async (backend) => {
          await runBrowserScenario(daemon, backend);
        });
      }, { defaultRuntime: true }),
      E2E_TEST_TIMEOUT_MS,
      `default-runtime real-daemon e2e did not finish within ${E2E_TEST_TIMEOUT_MS}ms`,
    );
  },
  E2E_TEST_TIMEOUT_MS + BUN_TEST_TIMEOUT_BACKSTOP_MARGIN_MS,
);

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

    const governance = await client.call("host.governance.inspect", null);
    expect(Object.keys(governance).sort()).toEqual([
      "approval_key_reference",
      "enrollment",
      "host_id",
      "owner",
      "owner_revision",
      "quarantine",
    ]);
    expect(/^host_[A-Za-z0-9_-]{43}$/.test(governance.host_id)).toBe(true);
    expect(/^approval_key_[A-Za-z0-9_-]{43}$/.test(governance.approval_key_reference)).toBe(true);
    expect(governance.enrollment).toBe(null);
    expect(governance.owner).toBe(null);
    expect(governance.owner_revision).toBe(null);
    expect(governance.quarantine).toBe(null);

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


async function withDaemon<T>(
  run: (daemon: DaemonHarness) => Promise<T>,
  options: DaemonOptions = {},
): Promise<T> {
  return withResource(
    await startDaemon(options),
    run,
    (daemon) => daemon.stop(),
    "e2e scenario and daemon teardown both failed",
  );
}

async function withBackend<T>(
  daemon: DaemonHarness,
  run: (backend: BackendHandle) => Promise<T>,
): Promise<T> {
  // No socket override: the backend must resolve the socket the daemon bound
  // from the same environment, with the production resolver.
  const backendEnv: NodeJS.ProcessEnv = {
    POHUNEK_BACKEND_BIND_HOST: LOOPBACK_HOST,
    POHUNEK_BACKEND_PORT: "0",
    POHUNEK_BACKEND_ALLOW_LOOPBACK: "1",
    POHUNEK_BACKEND_DISCOVER_INTERVAL: BACKEND_DISCOVER_INTERVAL_SECONDS,
    POHUNEK_BACKEND_STATIC_DIR: daemon.tempRoot,
    ...(daemon.env["XDG_RUNTIME_DIR"] === undefined
      ? {}
      : { XDG_RUNTIME_DIR: daemon.env["XDG_RUNTIME_DIR"] }),
  };
  expect(loadBackendConfig(backendEnv).daemonSocketPath).toBe(daemon.socketPath);
  const backend = await startBackendFromEnv(backendEnv, silentLogger);
  return withResource(
    backend,
    run,
    (started) => started.close(),
    "e2e scenario and backend teardown both failed",
  );
}

interface DaemonOptions {
  /** Leave `XDG_RUNTIME_DIR` unset so the daemon picks its platform default runtime directory. */
  readonly defaultRuntime?: boolean;
}


async function startDaemon(options: DaemonOptions = {}): Promise<DaemonHarness> {
  const tempRoot = await createFixtureRoot("pk-be-");
  const dirs = {
    runtime: join(tempRoot, "runtime"),
    data: join(tempRoot, "data"),
    state: join(tempRoot, "state"),
    cache: join(tempRoot, "cache"),
    config: join(tempRoot, "config"),
    home: join(tempRoot, "home"),
    bin: join(tempRoot, "bin"),
  };
  await Promise.all(
    Object.values(dirs).map((dir) => mkdir(dir, { recursive: true, mode: 0o700 })),
  );
  await writeFile(join(dirs.bin, "netbird"), NETBIRD_FIXTURE_SCRIPT, { mode: 0o700 });
  await writeFile(join(dirs.bin, "git"), GIT_FIXTURE_SCRIPT, { mode: 0o700 });

  const daemonBin = daemonBinaryPath();
  await access(daemonBin, constants.X_OK);
  const worker = await startDurableWorkerFixture({ daemonBin });
  const defaultRuntime = options.defaultRuntime === true
    ? await claimDefaultRuntimeDir()
    : undefined;
  const defaultRuntimeDir = defaultRuntime?.path;
  const isolatedEnv: NodeJS.ProcessEnv = {
    ...(defaultRuntimeDir === undefined ? { XDG_RUNTIME_DIR: dirs.runtime } : {}),
    XDG_DATA_HOME: dirs.data,
    XDG_STATE_HOME: dirs.state,
    XDG_CACHE_HOME: dirs.cache,
    XDG_CONFIG_HOME: dirs.config,
    HOME: dirs.home,
    PATH: dirs.bin,
    SHELL: "/bin/sh",
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
    ...worker.env,
  };
  const env = { ...withoutRuntimeDir(process.env, defaultRuntimeDir !== undefined), ...isolatedEnv };
  const socketPath = defaultRuntimeDir === undefined
    ? join(dirs.runtime, APP_DIR, SOCKET_NAME)
    : join(defaultRuntimeDir, SOCKET_NAME);
  const removeRoots = async (): Promise<void> => {
    // Independent: a failure removing the fixture root must not leak the default runtime directory.
    const results = await Promise.allSettled([
      rm(tempRoot, { recursive: true, force: true }),
      defaultRuntime === undefined ? Promise.resolve() : releaseDefaultRuntimeDir(defaultRuntime),
    ]);
    const failures = results
      .filter((result): result is PromiseRejectedResult => result.status === "rejected")
      .map((result) => result.reason as unknown);
    if (failures.length > 0) {
      throw new AggregateError(failures, "removing the daemon fixture roots failed");
    }
  };

  const daemon = await startDaemonProcess({ daemonBin, cwd: tempRoot, env, socketPath, removeRoots });
  return {
    tempRoot,
    env,
    socketPath: daemon.socketPath,
    stdout: () => daemon.stdout(),
    stderr: () => daemon.stderr(),
    stop: () => daemon.stop(),
  };
}

/** A default runtime directory this run created, identified by device and inode. */
interface DefaultRuntimeClaim {
  readonly path: string;
  readonly dev: number;
  readonly ino: number;
}

/**
 * Creates the macOS default runtime directory atomically (mode 0700), so this
 * run provably owns it: an existing directory belongs to a daemon of the same
 * user and `mkdir` fails instead of adopting it.
 */
async function claimDefaultRuntimeDir(): Promise<DefaultRuntimeClaim> {
  const uid = process.geteuid?.();
  if (uid === undefined) {
    throw new Error("the effective user id is unavailable");
  }
  const path = `${MACOS_DEFAULT_RUNTIME_PREFIX}${String(uid)}`;
  try {
    await mkdir(path, { mode: 0o700 });
  } catch (error: unknown) {
    if (error instanceof Error && "code" in error && error.code === "EEXIST") {
      throw new Error(
        `${path} already exists; stop the daemon using the default runtime directory `
          + "before running the default-runtime e2e",
      );
    }
    throw error;
  }
  const info = await lstat(path);
  return { path, dev: info.dev, ino: info.ino };
}

/** Removes the claimed directory, and only that directory: a replaced one is left alone. */
async function releaseDefaultRuntimeDir(claim: DefaultRuntimeClaim): Promise<void> {
  const info = await lstat(claim.path).catch(() => undefined);
  if (info === undefined) {
    return;
  }
  if (!info.isDirectory() || info.dev !== claim.dev || info.ino !== claim.ino) {
    throw new Error(`${claim.path} is no longer the directory this run created; not removing it`);
  }
  await rm(claim.path, { recursive: true, force: true });
}

function withoutRuntimeDir(env: NodeJS.ProcessEnv, remove: boolean): NodeJS.ProcessEnv {
  if (!remove) {
    return env;
  }
  const rest = { ...env };
  delete rest["XDG_RUNTIME_DIR"];
  return rest;
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
    v: SUPPORTED_PROTOCOL_VERSIONS,
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

function readHosts(response: Response): Promise<readonly BackendHostEntry[]> {
  return response.json() as Promise<readonly BackendHostEntry[]>;
}

function connectOptions(): { connectTimeoutMs: number; requestTimeoutMs: number } {
  return {
    connectTimeoutMs: DAEMON_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: DAEMON_REQUEST_TIMEOUT_MS,
  };
}
