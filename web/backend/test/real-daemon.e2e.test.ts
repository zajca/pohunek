import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
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
  SUPPORTED_PROTOCOL_VERSIONS,
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
const HERMES_PLUGIN_ASSETS = join(
  REPO_ROOT,
  "crates",
  "cli",
  "src",
  "hermes_integration",
  "assets",
);
const HERMES_PLUGIN_FIXTURE = join(TEST_DIR, "fixtures", "hermes-plugin-e2e.py");
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
const PLUGIN_E2E_TIMEOUT_MS = 60_000;
const PROGRAM_TIMEOUT_MS = 15_000;
const PLUGIN_FIXTURE_TIMEOUT_MS = 45_000;
const REMOTE_READY_TIMEOUT_MS = 10_000;
const REMOTE_PORT = 38_471;
const REMOTE_SELF_IP = "100.64.0.10";
const REMOTE_HOST = "plugin-remote.netbird.test";
const BACKEND_DISCOVER_INTERVAL_SECONDS = "60";
const MARKER = "pohunek-backend-real-daemon-e2e-marker";
// The worker needs a short window to observe the spawned controlled executable before it
// accepts its owner-private launch identity. This remains far below the fixture timeout.
const CONTROLLED_HERMES_IDENTITY_RETRY_ATTEMPTS = 40;
const CONTROLLED_HERMES_IDENTITY_TOTAL_CEILING_MS = 2_000;
const CONTROLLED_HERMES_IDENTITY_STEP_BUDGET_MS = 50;
const CONTROLLED_HERMES_IDENTITY_RETRY_DELAY_MS = 25;
const SAFE_AGENT_PREFERENCE = ["shell"] as const;
const NETBIRD_FIXTURE_SCRIPT = `#!/bin/sh
if [ "$1" != "status" ] || [ "$2" != "--json" ]; then
  exit 2
fi
printf '%s\\n' '{"daemonStatus":"Connected","peers":{"details":[]}}'
`;
const GIT_FIXTURE_SCRIPT = `#!/bin/sh
exec /usr/bin/git "$@"
`;
const PLUGIN_NETBIRD_FIXTURE_SCRIPT = `#!/bin/sh
if [ "$1" != "status" ] || [ "$2" != "--json" ]; then
  exit 2
fi
printf '%s\\n' '{"netbirdIp":"${REMOTE_SELF_IP}","daemonStatus":"Connected","peers":{"details":[{"fqdn":"${REMOTE_HOST}","netbirdIp":"${REMOTE_SELF_IP}","status":"Connected"}]}}'
`;
const encoder = new TextEncoder();

const silentLogger: BackendLogger = {
  log(): void {},
};

interface DaemonHarness {
  readonly socketPath: string;
  readonly tempRoot: string;
  readonly env: NodeJS.ProcessEnv;
  stdout(): string;
  stderr(): string;
  stop(): Promise<void>;
}

interface PluginDaemonHarness extends DaemonHarness {
  readonly pythonBin: string;
  readonly cliWrapper: string;
  readonly remoteCapture: string;
}

interface PluginPrerequisites {
  readonly cliBin: string;
  readonly pythonBin: string;
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

realDaemonTest("embedded Hermes plugin tools control a real durable shell session", async () => {
  const prerequisites = await pluginPrerequisites();
  await withTimeout(
    withPluginDaemon(prerequisites, async (daemon) => {
      await runHermesPluginScenario(daemon);
    }),
    PLUGIN_E2E_TIMEOUT_MS,
    `Hermes plugin real-daemon e2e did not finish within ${PLUGIN_E2E_TIMEOUT_MS}ms`,
  );
});

async function runHermesPluginScenario(daemon: PluginDaemonHarness): Promise<void> {
  await waitForRemoteListener(daemon.remoteCapture);
  const projectId = await createPluginProject(daemon, daemon.cliWrapper);
  const policyPath = join(daemon.tempRoot, "hermes-plugin-policy.json");
  await writeFile(
    policyPath,
    JSON.stringify({
      schema_version: 1,
      pohunek_cli: daemon.cliWrapper,
      protocol_min: PROTOCOL_VERSION,
      protocol_max: PROTOCOL_VERSION,
      access_mode: "full",
      allowed_hosts: [LOCAL_HOST, REMOTE_HOST],
      tool_timeout_ms: 8_000,
      max_output_bytes: 262_144,
      max_screen_bytes: 65_536,
      max_concurrency: 1,
    }),
    { mode: 0o600 },
  );

  const fixture = await runProgram(
    daemon.pythonBin,
    [
      HERMES_PLUGIN_FIXTURE,
      "--assets-dir",
      HERMES_PLUGIN_ASSETS,
      "--policy",
      policyPath,
      "--project-id",
      projectId,
      "--remote-host",
      REMOTE_HOST,
    ],
    pluginFixtureEnvironment(daemon.env),
    daemon.tempRoot,
    PLUGIN_FIXTURE_TIMEOUT_MS,
  );
  if (fixture.code !== 0) {
    throw new Error(
      `controlled Hermes plugin fixture failed (code=${String(fixture.code)}, `
        + `signal=${String(fixture.signal)}): ${fixtureFailureDiagnostic(fixture.stdout)}`,
    );
  }
  expect(fixture.signal === null).toBe(true);
  const result = parseFixtureResult(fixture.stdout);
  expect(result.ok).toBe(true);
  expect(result.logical_id_present).toBe(true);
  expect(result.runtime_id_present).toBe(true);
  expect(result.tools_exercised).toBe(16);
  expect(result.origin_denials).toBe(8);
  expect(result.hermes_resume).toBe(true);
  expect(result.hermes_fork_unsupported).toBe(true);
  expect(result.output_gap_recovered).toBe(true);
  expect(result.remote_loopback).toBe(true);
  await assertRemoteTargets(daemon.remoteCapture, `${REMOTE_SELF_IP}:${REMOTE_PORT}`);
}

async function createPluginProject(daemon: DaemonHarness, cli: string): Promise<string> {
  const repository = join(daemon.tempRoot, "hermes-plugin-project");
  await mkdir(repository, { recursive: true });
  const initialized = await runProgram(
    "git",
    ["init", "--initial-branch=main", repository],
    daemon.env,
    daemon.tempRoot,
  );
  if (initialized.code !== 0 || initialized.signal !== null) {
    throw new Error("controlled local git repository setup failed");
  }
  await writeFile(join(repository, "fixture.txt"), "controlled plugin fixture\n", { mode: 0o600 });
  await requireProgramSuccess(
    "git",
    ["-C", repository, "add", "fixture.txt"],
    daemon.env,
    daemon.tempRoot,
    "controlled local git add",
  );
  await requireProgramSuccess(
    "git",
    [
      "-C", repository,
      "-c", "user.name=Pohunek E2E",
      "-c", "user.email=pohunek-e2e@example.invalid",
      "commit", "-m", "Initialize fixture repository",
    ],
    daemon.env,
    daemon.tempRoot,
    "controlled local git commit",
  );
  const added = await runProgram(
    cli,
    ["project", "add", repository, "--name", "hermes-plugin-e2e", "--json"],
    daemon.env,
    daemon.tempRoot,
  );
  if (added.code !== 0 || added.signal !== null) {
    throw new Error("controlled plugin project registration failed");
  }
  const envelope = parseJsonEnvelope(added.stdout);
  return requireIdentifier(envelope.ok, "project id");
}

async function pluginPrerequisites(): Promise<PluginPrerequisites> {
  return {
    cliBin: await requiredExecutable("POHUNEK_CLI_BIN"),
    pythonBin: await requiredExecutable("POHUNEK_PYTHON_BIN"),
  };
}

async function requiredExecutable(variable: string): Promise<string> {
  const configured = process.env[variable];
  if (configured === undefined || configured.length === 0) {
    throw new Error(`${variable} is required when POHUNEK_E2E=1`);
  }
  if (!isAbsolute(configured)) {
    throw new Error(`${variable} must be an absolute path`);
  }
  await access(configured, constants.X_OK);
  return configured;
}

interface ProgramResult {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
  readonly stdout: string;
}

async function runProgram(
  command: string,
  args: readonly string[],
  env: NodeJS.ProcessEnv,
  cwd: string,
  timeoutMs = PROGRAM_TIMEOUT_MS,
): Promise<ProgramResult> {
  const child = spawn(command, args, {
    cwd,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const stdout: string[] = [];
  let stdoutBytes = 0;
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk: string): void => {
    stdoutBytes += Buffer.byteLength(chunk);
    if (stdoutBytes > 1_048_576) {
      child.kill("SIGKILL");
      return;
    }
    stdout.push(chunk);
  });
  child.stderr.resume();
  let exit: unknown[];
  try {
    exit = await withTimeout(
      once(child, "exit"),
      timeoutMs,
      `controlled process did not finish within ${timeoutMs}ms`,
    );
  } catch (error: unknown) {
    child.kill("SIGKILL");
    await once(child, "exit").catch(() => undefined);
    throw error;
  }
  if (stdoutBytes > 1_048_576) {
    throw new Error("controlled process exceeded the stdout cap");
  }
  return {
    code: exit[0] as number | null,
    signal: exit[1] as NodeJS.Signals | null,
    stdout: stdout.join(""),
  };
}

function pluginFixtureEnvironment(source: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const selected: NodeJS.ProcessEnv = {};
  for (const name of [
    "HOME", "XDG_RUNTIME_DIR", "XDG_DATA_HOME", "XDG_STATE_HOME",
    "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "PATH", "SHELL", "LANG", "LC_ALL",
  ]) {
    const value = source[name];
    if (value !== undefined) selected[name] = value;
  }
  return selected;
}

async function waitForRemoteListener(capture: string): Promise<void> {
  const expected = `bind ${REMOTE_SELF_IP}:${REMOTE_PORT}`;
  const deadline = Date.now() + REMOTE_READY_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const contents = await readFile(capture, "utf8").catch(() => "");
    if (contents.split("\n").includes(expected)) return;
    await delay(DAEMON_READY_POLL_MS);
  }
  throw new Error("controlled remote listener did not bind the original CGNAT target");
}

async function assertRemoteTargets(capture: string, target: string): Promise<void> {
  const lines = (await readFile(capture, "utf8")).trim().split("\n");
  expect(lines.filter((line) => line === `bind ${target}`).length > 0).toBe(true);
  expect(lines.filter((line) => line === `connect ${target}`).length > 2).toBe(true);
}

interface JsonEnvelope {
  readonly ok: unknown;
}

function parseJsonEnvelope(stdout: string): JsonEnvelope {
  const parsed: unknown = JSON.parse(stdout);
  if (!isRecord(parsed) || !("ok" in parsed) || "err" in parsed) {
    throw new Error("controlled CLI did not emit a success JSON envelope");
  }
  return { ok: parsed["ok"] };
}

function requireIdentifier(value: unknown, label: string): string {
  if (!isRecord(value) || typeof value["id"] !== "string" || value["id"].length === 0) {
    throw new Error(`controlled CLI response omitted ${label}`);
  }
  return value["id"];
}

interface FixtureResult {
  readonly ok: boolean;
  readonly logical_id_present: boolean;
  readonly runtime_id_present: boolean;
  readonly tools_exercised: number;
  readonly origin_denials: number;
  readonly hermes_resume: boolean;
  readonly hermes_fork_unsupported: boolean;
  readonly output_gap_recovered: boolean;
  readonly remote_loopback: boolean;
}

/**
 * Return the fixture's deliberately payload-free failure reason for E2E diagnosis.
 *
 * The controlled Python fixture emits only bounded, hand-authored assertion labels;
 * terminal output, paths, identifiers, and daemon logs never cross this boundary.
 */
function fixtureFailureDiagnostic(stdout: string): string {
  try {
    const parsed: unknown = JSON.parse(stdout);
    if (isRecord(parsed) && parsed["ok"] === false && typeof parsed["error"] === "string") {
      return parsed["error"].slice(0, 160);
    }
  } catch {
    // The diagnostic remains deliberately generic for malformed fixture output.
  }
  return "fixture emitted no safe diagnostic";
}

function parseFixtureResult(stdout: string): FixtureResult {
  const parsed: unknown = JSON.parse(stdout);
  if (
    !isRecord(parsed)
    || typeof parsed["ok"] !== "boolean"
    || typeof parsed["logical_id_present"] !== "boolean"
    || typeof parsed["runtime_id_present"] !== "boolean"
    || typeof parsed["tools_exercised"] !== "number"
    || typeof parsed["origin_denials"] !== "number"
    || typeof parsed["hermes_resume"] !== "boolean"
    || typeof parsed["hermes_fork_unsupported"] !== "boolean"
    || typeof parsed["output_gap_recovered"] !== "boolean"
    || typeof parsed["remote_loopback"] !== "boolean"
  ) {
    throw new Error("Hermes plugin fixture emitted an invalid result");
  }
  return {
    ok: parsed["ok"],
    logical_id_present: parsed["logical_id_present"],
    runtime_id_present: parsed["runtime_id_present"],
    tools_exercised: parsed["tools_exercised"],
    origin_denials: parsed["origin_denials"],
    hermes_resume: parsed["hermes_resume"],
    hermes_fork_unsupported: parsed["hermes_fork_unsupported"],
    output_gap_recovered: parsed["output_gap_recovered"],
    remote_loopback: parsed["remote_loopback"],
  };
}

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

async function withPluginDaemon<T>(
  prerequisites: PluginPrerequisites,
  run: (daemon: PluginDaemonHarness) => Promise<T>,
): Promise<T> {
  const daemon = await startDaemon(prerequisites);
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
      throw new AggregateError([failure, error], "plugin e2e and daemon teardown both failed");
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

function startDaemon(): Promise<DaemonHarness>;
function startDaemon(plugin: PluginPrerequisites): Promise<PluginDaemonHarness>;
async function startDaemon(
  plugin?: PluginPrerequisites,
): Promise<DaemonHarness | PluginDaemonHarness> {
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
  await writeFile(
    join(dirs.bin, "netbird"),
    plugin === undefined ? NETBIRD_FIXTURE_SCRIPT : PLUGIN_NETBIRD_FIXTURE_SCRIPT,
    { mode: 0o700 },
  );
  await writeFile(join(dirs.bin, "git"), GIT_FIXTURE_SCRIPT, { mode: 0o700 });

  const daemonBin = daemonBinaryPath();
  await access(daemonBin, constants.X_OK);
  const worker = plugin === undefined
    ? await startDurableWorkerFixture({ daemonBin })
    : await isolatedDurableWorkerFixture(daemonBin);
  const controlled = plugin === undefined
    ? undefined
    : await preparePluginExecutables(plugin, tempRoot, dirs);
  const stdoutChunks: string[] = [];
  const stderrChunks: string[] = [];
  let exitStatus: ExitStatus | undefined;
  let spawnError: Error | undefined;

  const isolatedEnv: NodeJS.ProcessEnv = {
    XDG_RUNTIME_DIR: dirs.runtime,
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
    ...(controlled === undefined ? {} : {
      LD_PRELOAD: controlled.interposer,
      POHUNEK_TEST_SOCKET_CAPTURE: controlled.capture,
      POHUNEK_REMOTE_PORT: String(REMOTE_PORT),
    }),
  };
  const daemonEnv = plugin === undefined
    ? { ...process.env, ...isolatedEnv }
    : isolatedEnv;
  const child = spawn(daemonBin, [], {
    cwd: tempRoot,
    env: daemonEnv,
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
    await rm(tempRoot, { recursive: true, force: true });
    throw error;
  }

  return {
    socketPath,
    tempRoot,
    env: daemonEnv,
    ...(controlled === undefined ? {} : {
      pythonBin: controlled.pythonBin,
      cliWrapper: controlled.cliWrapper,
      remoteCapture: controlled.capture,
    }),
    ...logs(),
    stop: async (): Promise<void> => {
      const status = await stopChild(child, exitPromise, () => exitStatus);
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

async function isolatedDurableWorkerFixture(
  daemonBin: string,
): Promise<{ readonly env: Readonly<Record<string, string>> }> {
  const workerBin = join(dirname(daemonBin), "pohunek-sessiond");
  await access(workerBin, constants.X_OK);
  return {
    env: {
      POHUNEK_WORKER_LAUNCHER: "subprocess",
      POHUNEK_WORKER_BIN: workerBin,
    },
  };
}

interface ControlledPluginExecutables {
  readonly interposer: string;
  readonly capture: string;
  readonly cliWrapper: string;
  readonly pythonBin: string;
}

async function preparePluginExecutables(
  prerequisites: PluginPrerequisites,
  tempRoot: string,
  dirs: {
    readonly runtime: string;
    readonly data: string;
    readonly state: string;
    readonly cache: string;
    readonly config: string;
    readonly home: string;
    readonly bin: string;
  },
): Promise<ControlledPluginExecutables> {
  const compiler = "/usr/bin/cc";
  await access(compiler, constants.X_OK);
  const buildEnv: NodeJS.ProcessEnv = {
    HOME: dirs.home,
    PATH: "/usr/bin:/bin",
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
  };
  const interposerSource = join(tempRoot, "socket-redirect.c");
  const interposer = join(tempRoot, "socket-redirect.so");
  const capture = join(tempRoot, "socket-targets.log");
  await writeFile(interposerSource, SOCKET_REDIRECT_SOURCE, { mode: 0o600 });
  await requireProgramSuccess(
    compiler,
    ["-shared", "-fPIC", "-O2", interposerSource, "-o", interposer, "-ldl"],
    buildEnv,
    tempRoot,
    "socket interposer compilation",
  );

  const hermesSource = join(tempRoot, "controlled-hermes.c");
  const hermes = join(dirs.bin, "hermes");
  await writeFile(hermesSource, CONTROLLED_HERMES_SOURCE, { mode: 0o600 });
  await requireProgramSuccess(
    compiler,
    ["-O2", hermesSource, "-o", hermes],
    buildEnv,
    tempRoot,
    "controlled Hermes compilation",
  );

  const cliWrapper = join(dirs.bin, "pohunek-plugin-cli");
  const wrapper = `#!/bin/sh
exec /usr/bin/env -i \
  HOME=${shellQuote(dirs.home)} \
  XDG_RUNTIME_DIR=${shellQuote(dirs.runtime)} \
  XDG_DATA_HOME=${shellQuote(dirs.data)} \
  XDG_STATE_HOME=${shellQuote(dirs.state)} \
  XDG_CACHE_HOME=${shellQuote(dirs.cache)} \
  XDG_CONFIG_HOME=${shellQuote(dirs.config)} \
  PATH=${shellQuote(dirs.bin)} \
  SHELL=/bin/sh LANG=C.UTF-8 LC_ALL=C.UTF-8 \
  LD_PRELOAD=${shellQuote(interposer)} \
  POHUNEK_TEST_SOCKET_CAPTURE=${shellQuote(capture)} \
  POHUNEK_REMOTE_PORT=${String(REMOTE_PORT)} \
  ${shellQuote(prerequisites.cliBin)} "$@"
`;
  await writeFile(cliWrapper, wrapper, { mode: 0o700 });
  return { interposer, capture, cliWrapper, pythonBin: prerequisites.pythonBin };
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

async function requireProgramSuccess(
  command: string,
  args: readonly string[],
  env: NodeJS.ProcessEnv,
  cwd: string,
  operation: string,
): Promise<void> {
  const result = await runProgram(command, args, env, cwd);
  if (result.code !== 0 || result.signal !== null) {
    throw new Error(`${operation} failed`);
  }
}

const SOCKET_REDIRECT_SOURCE = String.raw`#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

typedef int (*socket_fn)(int, const struct sockaddr *, socklen_t);

static void capture_target(const char *operation, const struct sockaddr_in *original) {
    const char *capture = getenv("POHUNEK_TEST_SOCKET_CAPTURE");
    if (capture == NULL) return;
    char ip[INET_ADDRSTRLEN];
    char line[112];
    inet_ntop(AF_INET, &original->sin_addr, ip, sizeof(ip));
    int count = snprintf(line, sizeof(line), "%s %s:%u\n", operation, ip,
                         ntohs(original->sin_port));
    int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (output >= 0) {
        (void)write(output, line, (size_t)count);
        (void)close(output);
    }
}

static int redirect(socket_fn real_call, const char *operation, int fd,
                    const struct sockaddr *address, socklen_t length) {
    if (address != NULL && address->sa_family == AF_INET &&
        length >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *original = (const struct sockaddr_in *)address;
        unsigned int host = ntohl(original->sin_addr.s_addr);
        if ((host & 0xffc00000U) == 0x64400000U) {
            capture_target(operation, original);
            struct sockaddr_in redirected = *original;
            redirected.sin_addr.s_addr = htonl(0x7f000001U);
            return real_call(fd, (const struct sockaddr *)&redirected, sizeof(redirected));
        }
    }
    return real_call(fd, address, length);
}

int connect(int fd, const struct sockaddr *address, socklen_t length) {
    static socket_fn real_connect = NULL;
    if (real_connect == NULL) real_connect = (socket_fn)dlsym(RTLD_NEXT, "connect");
    return redirect(real_connect, "connect", fd, address, length);
}

int bind(int fd, const struct sockaddr *address, socklen_t length) {
    static socket_fn real_bind = NULL;
    if (real_bind == NULL) real_bind = (socket_fn)dlsym(RTLD_NEXT, "bind");
    return redirect(real_bind, "bind", fd, address, length);
}
`;

const CONTROLLED_HERMES_SOURCE = String.raw`#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

enum {
    IDENTITY_RETRY_ATTEMPTS = ${CONTROLLED_HERMES_IDENTITY_RETRY_ATTEMPTS},
    IDENTITY_TOTAL_CEILING_MS = ${CONTROLLED_HERMES_IDENTITY_TOTAL_CEILING_MS},
    IDENTITY_STEP_BUDGET_MS = ${CONTROLLED_HERMES_IDENTITY_STEP_BUDGET_MS},
    IDENTITY_RETRY_DELAY_MS = ${CONTROLLED_HERMES_IDENTITY_RETRY_DELAY_MS},
    IDENTITY_RESPONSE_MAX_BYTES = 512,
};

enum hook_response {
    HOOK_RESPONSE_TERMINAL = -1,
    HOOK_RESPONSE_RETRY = 0,
    HOOK_RESPONSE_ACCEPTED = 1,
};

static long long monotonic_millis(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (long long)now.tv_sec * 1000LL + now.tv_nsec / 1000000LL;
}

static int remaining_millis(long long deadline) {
    long long now = monotonic_millis();
    if (now < 0 || now >= deadline) return 0;
    long long remaining = deadline - now;
    return remaining > INT_MAX ? INT_MAX : (int)remaining;
}

static int wait_for_fd(int fd, short events, long long total_deadline) {
    long long now = monotonic_millis();
    if (now < 0 || now >= total_deadline) return 0;
    long long candidate_deadline = now + IDENTITY_STEP_BUDGET_MS;
    long long step_deadline = candidate_deadline < total_deadline
        ? candidate_deadline : total_deadline;
    for (;;) {
        int remaining = remaining_millis(step_deadline);
        if (remaining == 0) return 0;
        struct pollfd descriptor = { .fd = fd, .events = events, .revents = 0 };
        int ready = poll(&descriptor, 1, remaining);
        if (ready > 0) {
            if ((descriptor.revents & POLLNVAL) != 0) return -1;
            if ((descriptor.revents & (events | POLLERR | POLLHUP)) != 0) return 1;
            return -1;
        }
        if (ready == 0) return 0;
        if (errno != EINTR) return -1;
    }
}

static void wait_before_retry(long long total_deadline) {
    long long now = monotonic_millis();
    if (now < 0 || now >= total_deadline) return;
    long long candidate_deadline = now + IDENTITY_RETRY_DELAY_MS;
    long long retry_deadline = candidate_deadline < total_deadline
        ? candidate_deadline : total_deadline;
    for (;;) {
        int remaining = remaining_millis(retry_deadline);
        if (remaining == 0) return;
        int result = poll(NULL, 0, remaining);
        if (result == 0) return;
        if (result < 0 && errno == EINTR) continue;
        return;
    }
}

static unsigned long long process_start_identity(void) {
    FILE *handle = fopen("/proc/self/stat", "r");
    char buffer[4096];
    if (handle == NULL) return 0;
    if (fgets(buffer, sizeof(buffer), handle) == NULL) {
        fclose(handle);
        return 0;
    }
    fclose(handle);
    char *cursor = strrchr(buffer, ')');
    if (cursor == NULL) return 0;
    cursor += 2;
    char *save = NULL;
    char *token = strtok_r(cursor, " ", &save);
    for (int field = 3; token != NULL; field++, token = strtok_r(NULL, " ", &save)) {
        if (field == 22) return strtoull(token, NULL, 10);
    }
    return 0;
}

static enum hook_response parse_hook_response(const char *response) {
    static const char accepted[] =
        "{\"ok\":true,\"launch_identity_accepted\":true}\n";
    static const char retryable[] =
        "{\"ok\":true,\"launch_identity_accepted\":false}\n";
    static const char rejected[] =
        "{\"ok\":false,\"launch_identity_accepted\":false}\n";
    if (strcmp(response, accepted) == 0) return HOOK_RESPONSE_ACCEPTED;
    if (strcmp(response, retryable) == 0) return HOOK_RESPONSE_RETRY;
    if (strcmp(response, rejected) == 0) return HOOK_RESPONSE_TERMINAL;
    return HOOK_RESPONSE_TERMINAL;
}

static int connect_before_send(
    int fd,
    const struct sockaddr_un *address,
    socklen_t address_length,
    long long total_deadline
) {
    if (connect(fd, (const struct sockaddr *)address, address_length) == 0) return 1;
    if (errno != EINPROGRESS) return 0;
    int ready = wait_for_fd(fd, POLLOUT, total_deadline);
    if (ready <= 0) return 0;
    int socket_error = 0;
    socklen_t error_length = sizeof(socket_error);
    if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &socket_error, &error_length) != 0) return 0;
    return socket_error == 0;
}

static int send_request(
    int fd,
    const char *request,
    size_t request_length,
    long long total_deadline
) {
    size_t sent_bytes = 0;
    while (sent_bytes < request_length) {
        ssize_t sent = send(
            fd,
            request + sent_bytes,
            request_length - sent_bytes,
            MSG_NOSIGNAL
        );
        if (sent > 0) {
            sent_bytes += (size_t)sent;
            continue;
        }
        if (sent == 0) return 0;
        if (errno == EINTR) {
            if (remaining_millis(total_deadline) == 0) return 0;
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) return 0;
        if (wait_for_fd(fd, POLLOUT, total_deadline) != 1) return 0;
    }
    return 1;
}

static enum hook_response read_response(int fd, long long total_deadline) {
    char response[IDENTITY_RESPONSE_MAX_BYTES];
    size_t response_length = 0;
    int newline_seen = 0;
    for (;;) {
        if (response_length == sizeof(response) - 1) return HOOK_RESPONSE_TERMINAL;
        ssize_t received = recv(
            fd,
            response + response_length,
            sizeof(response) - 1 - response_length,
            0
        );
        if (received > 0) {
            size_t chunk_length = (size_t)received;
            const char *newline = memchr(response + response_length, '\n', chunk_length);
            if (newline_seen || (newline != NULL && newline != response + response_length + chunk_length - 1)) {
                return HOOK_RESPONSE_TERMINAL;
            }
            response_length += chunk_length;
            if (newline != NULL) newline_seen = 1;
            continue;
        }
        if (received == 0) {
            if (!newline_seen) return HOOK_RESPONSE_TERMINAL;
            response[response_length] = '\0';
            return parse_hook_response(response);
        }
        if (errno == EINTR) {
            if (remaining_millis(total_deadline) == 0) return HOOK_RESPONSE_TERMINAL;
            continue;
        }
        if (errno != EAGAIN && errno != EWOULDBLOCK) return HOOK_RESPONSE_TERMINAL;
        if (wait_for_fd(fd, POLLIN, total_deadline) != 1) return HOOK_RESPONSE_TERMINAL;
    }
}

static int report_identity(void) {
    const char *endpoint = getenv("POHUNEK_WORKER_SOCKET_PATH");
    const char *runtime = getenv("POHUNEK_RUNTIME_ID");
    if (endpoint == NULL || runtime == NULL) return 0;
    unsigned long long start = process_start_identity();
    if (start == 0) return 0;
    time_t current_time = time(NULL);
    if (current_time == (time_t)-1) return 0;
    time_t expiry_time = current_time + 30;
    struct tm expiry;
    char expires[32];
    if (gmtime_r(&expiry_time, &expiry) == NULL) return 0;
    if (strftime(expires, sizeof(expires), "%Y-%m-%dT%H:%M:%SZ", &expiry) == 0) return 0;
    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    if (strlen(endpoint) >= sizeof(address.sun_path)) return 0;
    strcpy(address.sun_path, endpoint);
    socklen_t address_length = (socklen_t)(
        offsetof(struct sockaddr_un, sun_path) + strlen(address.sun_path) + 1
    );
    long long started = monotonic_millis();
    if (started < 0) return 0;
    long long total_deadline = started + IDENTITY_TOTAL_CEILING_MS;

    for (int attempt = 0; attempt < IDENTITY_RETRY_ATTEMPTS; attempt++) {
        if (remaining_millis(total_deadline) == 0) return 0;
        char request[2048];
        unsigned int sequence = (unsigned int)attempt + 1U;
        int request_length = snprintf(request, sizeof(request),
            "{\"type\":\"identity_report\",\"runtime_id\":\"%s\","
            "\"provider\":\"hermes\",\"pid\":%ld,\"start_identity\":%llu,"
            "\"sequence\":%u,\"expires_at\":\"%s\",\"reference_kind\":\"id\","
            "\"native_reference\":\"hermes-e2e-native\"}\n",
            runtime, (long)getpid(), start, sequence, expires);
        if (request_length <= 0 || (size_t)request_length >= sizeof(request)) return 0;

        // Each fresh sequence gets one fresh owner-private nonblocking connection. Only a
        // failure before any request byte is sent is safe to retry without a response.
        int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if (fd < 0) {
            wait_before_retry(total_deadline);
            continue;
        }
        if (!connect_before_send(fd, &address, address_length, total_deadline)) {
            close(fd);
            wait_before_retry(total_deadline);
            continue;
        }
        if (!send_request(fd, request, (size_t)request_length, total_deadline)) {
            close(fd);
            return 0;
        }
        enum hook_response response = read_response(fd, total_deadline);
        close(fd);
        if (response == HOOK_RESPONSE_ACCEPTED) return 1;
        if (response == HOOK_RESPONSE_TERMINAL) return 0;
        if (attempt + 1 < IDENTITY_RETRY_ATTEMPTS) wait_before_retry(total_deadline);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--version") == 0) {
        puts("Hermes Agent v0.20.0");
        return 0;
    }
    if (argc < 2 || strcmp(argv[1], "chat") != 0) return 2;
    int resumed = argc == 4 && strcmp(argv[2], "--resume") == 0 &&
                  strcmp(argv[3], "hermes-e2e-native") == 0;
    if (argc != 2 && !resumed) return 2;
    if (!report_identity()) {
        fputs("controlled Hermes launch identity was not accepted\n", stderr);
        return 1;
    }
    puts("hermes-e2e-ready");
    fflush(stdout);
    if (!resumed) {
        usleep(500000);
        return 0;
    }
    char input[4096];
    while (read(STDIN_FILENO, input, sizeof(input)) >= 0) pause();
    return 0;
}
`;

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
