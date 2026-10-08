import { afterAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { rm } from "node:fs/promises";
import { once } from "node:events";
import type { Readable } from "node:stream";
import { dirname, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { PROTOCOL_VERSION } from "@pohunek/protocol";
import {
  SOCKET_NAME,
  SOCKET_PATH_MAX_BYTES,
  type RuntimePathFailure,
} from "@pohunek/sdk";
import { createFixtureRootSync } from "@pohunek/testkit";
import {
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startUnixDaemon,
} from "./mock-daemon";

/**
 * The SDK runtime-path resolver against the shared Rust `pohunek-paths` fixture
 * and a real daemon: an isolated child process starts with a fixture case's
 * environment, resolves the daemon socket through the public SDK surface and
 * connects a real client to it. Resolution agreement with the paths crate and
 * the fail-closed taxonomy are therefore exercised across the real transport,
 * not through a direct resolver call.
 *
 * The shared fixture drives both implementations; this suite reaches the SDK
 * wherever it shares the runtime-directory surface. Cases that fail on the
 * Rust durable-state variables (XDG_DATA_HOME and friends) are hostile
 * environments the resolver must ignore, not resolver outcomes. The macOS
 * owner defaults would select a directory of the real host, so only cases
 * with an explicitly configured runtime directory are driven.
 */

const HOST_PLATFORM = process.platform === "darwin" ? "macos" : "linux";
const SDK_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
// Every resolver child must report its taxonomy long before the suite
// deadline; a child past this point has lost its way and is killed.
const CHILD_TIMEOUT_MS = 3_000;
const FIXTURE_PATH = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../crates/paths/fixtures/runtime-paths.json",
);

const CHILD_SCRIPT = `
const sdk = await import("@pohunek/sdk");
const done = (result) => process.stdout.write(JSON.stringify(result));
try {
  const socket = sdk.resolveDaemonSocket(sdk.currentRuntimePathContext(), process.env);
  const client = await sdk.connectLocal(socket, { connectTimeoutMs: 5000 });
  const health = await client.call("daemon.health", null);
  done({ resolved: socket, protocolVersion: health.protocol_version });
  await client.close();
} catch (error) {
  if (error !== null && typeof error === "object" && "failure" in error) {
    done({ failure: error.failure });
    process.exit(1);
  }
  done({ error: error instanceof Error ? error.message : String(error) });
  process.exit(2);
}
`;

const placeholderRoots: string[] = [];
afterAll(async (): Promise<void> => {
  await Promise.all(placeholderRoots.map((root) => rm(root, { recursive: true, force: true })));
});

describe("SDK runtime paths against the shared fixture", () => {
  test("resolves the fixture socket and connects a real client to it", async () => {
    const cases = readFixture().cases.filter(
      (fixtureCase) => fixtureCase.platform === HOST_PLATFORM && fixtureCase.env.XDG_RUNTIME_DIR !== undefined,
    );
    expect(cases.length).toBeGreaterThanOrEqual(1);

    for (const fixtureCase of cases) {
      const base = placeholder();
      // A daemon listens exactly where the paths crate documents the socket;
      // the child resolves from the environment alone and must land on the
      // same file to connect at all.
      const socket = fixtureSocketOf(fixtureCase, base);
      const daemon = await startUnixDaemon([healthReplyStep()], { socketPath: socket });
      try {
        const outcome = await runChild({ ...fixtureCase.env, XDG_RUNTIME_DIR: base });
        expect(outcome.code).toBe(0);
        expect(outcome.resolved).toBe(socket);
        expect(outcome.protocolVersion).toBe(PROTOCOL_VERSION);
        expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
        await daemon.expectNoRequest(50);
      } finally {
        await daemon.close();
      }
    }
  });

  test("a hostile durable-state environment still resolves and connects", async () => {
    // The fixture declares hostile durable-state variables (XDG_DATA_HOME and
    // friends) for the paths crate; the SDK shares only the runtime directory,
    // so resolution must ignore them and still reach the real socket.
    // A durable-state failure in the Rust fixture is safe for the SDK
    // runtime resolver on either host platform.
    const fixtureCase = readFixture().cases.find(
      (candidate) => candidate.name === "relative_explicit_state_home",
    );
    if (fixtureCase === undefined) {
      throw new Error("the fixture lost its durable-state-hostile case");
    }

    const base = placeholder();
    const socket = fixtureSocketOf(fixtureCase, base);
    const daemon = await startUnixDaemon([healthReplyStep()], { socketPath: socket });
    try {
      const outcome = await runChild({ ...fixtureCase.env, XDG_RUNTIME_DIR: base });
      expect(outcome.code).toBe(0);
      expect(outcome.resolved).toBe(socket);
      expect(outcome.protocolVersion).toBe(PROTOCOL_VERSION);
      expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
    } finally {
      await daemon.close();
    }
  });

  test("a hostile runtime environment fails closed before any socket dial", async () => {
    const limit = SOCKET_PATH_MAX_BYTES[HOST_PLATFORM];
    const hostile: [string, FixtureEnvironment, RuntimePathFailure][] = [
      [
        "a relative runtime directory",
        { XDG_RUNTIME_DIR: "relative/runtime", HOME: "/home/operator" },
        { variant: "invalid_env", variable: "XDG_RUNTIME_DIR", reason: "not_absolute" },
      ],
      [
        "a runtime directory whose socket exceeds the native sun_path budget",
        { XDG_RUNTIME_DIR: "/segment".repeat(limit + 1), HOME: "/home/operator" },
        {
          variant: "socket_path_invalid",
          variable: "XDG_RUNTIME_DIR",
          detail: `at most ${limit} bytes`,
        },
      ],
    ];
    // macOS has an owner default when XDG_RUNTIME_DIR is absent; Linux
    // requires the variable and must fail before attempting a connection.
    if (HOST_PLATFORM === "linux") {
      hostile.unshift([
        "a missing runtime directory",
        { HOME: "/home/operator" },
        { variant: "missing_env", variable: "XDG_RUNTIME_DIR" },
      ]);
    }

    for (const [name, environment, expected] of hostile) {
      const outcome = await runChild(environment);
      expect(`${name}: ${outcome.code}`).toBe(`${name}: 1`);
      expect(`${name}: ${outcome.failure?.variant ?? "missing"}`).toBe(
        `${name}: ${expected.variant}`,
      );
      expect(`${name}: ${outcome.failure?.variable ?? "missing"}`).toBe(
        `${name}: ${expected.variable}`,
      );
      if ("reason" in expected && expected.reason !== undefined) {
        expect(
          `${name}: ${outcome.failure !== undefined && "reason" in outcome.failure ? outcome.failure.reason : "missing"}`,
        ).toBe(`${name}: ${expected.reason}`);
      }
      if ("detail" in expected) {
        expect(childDetail(outcome)).toContain(expected.detail);
      }
    }
  });
});

/** Returns the resolver failure detail from a child outcome expected to carry one. */
function childDetail(outcome: ChildResult): string {
  const failure = outcome.failure;
  if (failure === undefined) {
    throw new Error("expected the child outcome to carry a failure");
  }
  return "detail" in failure ? failure.detail : JSON.stringify(failure);
}

interface FixtureEnvironment {
  readonly XDG_RUNTIME_DIR?: string;
  readonly XDG_DATA_HOME?: string;
  readonly XDG_STATE_HOME?: string;
  readonly XDG_CACHE_HOME?: string;
  readonly XDG_CONFIG_HOME?: string;
  readonly HOME?: string;
}

interface RuntimePathCase {
  readonly name: string;
  readonly platform: "linux" | "macos";
  readonly effective_uid: number;
  readonly env: FixtureEnvironment;
  readonly expected?: { readonly socket: string; readonly runtime_dir: string };
  readonly expected_runtime_dir?: string;
  readonly error?: { readonly variant: string; readonly var: string; readonly reason?: string };
}

interface ChildResult {
  readonly code: number | null;
  readonly resolved?: string;
  readonly protocolVersion?: number;
  readonly failure?: RuntimePathFailure;
}

function healthReplyStep(): { kind: "reply"; line: (requestLine: string) => string } {
  return {
    kind: "reply",
    line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), {
      status: "ok",
      daemon_version: "0.0.0-test",
      protocol_version: PROTOCOL_VERSION,
    }),
  };
}

function readFixture(): { readonly cases: RuntimePathCase[] } {
  const fixture = JSON.parse(readFileSync(FIXTURE_PATH, "utf8")) as {
    readonly cases: RuntimePathCase[];
  };
  const names = fixture.cases.map((entry) => entry.name);
  for (const required of [
    "linux_explicit_overrides",
    "linux_missing_runtime",
    "macos_default_alternate_uid",
    "macos_explicit_runtime_alternate_uid",
    "macos_runtime_and_home_defaults",
    "missing_home_for_durable_defaults",
    "empty_explicit_runtime",
    "relative_explicit_state_home",
  ]) {
    expect(names).toContain(required);
  }
  expect(fixture.cases.length).toBeGreaterThanOrEqual(10);
  return fixture;
}

/** The fixture's documented socket for `fixtureCase`, under the sandbox base. */
function fixtureSocketOf(fixtureCase: RuntimePathCase, base: string): string {
  const explicit = fixtureCase.env.XDG_RUNTIME_DIR;
  if (explicit === undefined) {
    throw new Error(`${fixtureCase.name} has no explicit runtime directory`);
  }
  // The contract rule the fixture documents for every explicit case: the
  // socket sits under `<XDG_RUNTIME_DIR>/pohunek`. Cross-check the fixture's
  // own documentation against that rule before substituting the base.
  const derived = posix.join(explicit, "pohunek", SOCKET_NAME);
  const documented = fixtureCase.expected?.socket
    ?? (fixtureCase.expected_runtime_dir !== undefined
      ? posix.join(fixtureCase.expected_runtime_dir, SOCKET_NAME)
      : undefined);
  expect(documented ?? derived).toBe(derived);
  return posix.join(base, "pohunek", SOCKET_NAME);
}

/**
 * Runs the resolver child asynchronously, with a bounded kill timeout:
 * `spawnSync` would block this event loop, and the daemon fixture that must
 * serve the child lives in this very process.
 */
async function runChild(environment: FixtureEnvironment): Promise<ChildResult> {
  const child = spawn(process.execPath, ["--eval", CHILD_SCRIPT], {
    cwd: SDK_ROOT,
    env: childEnv(environment),
    stdio: ["ignore", "pipe", "pipe"],
  });
  const killTimer = setTimeout((): void => {
    child.kill("SIGKILL");
    child.stdout?.destroy();
    child.stderr?.destroy();
  }, CHILD_TIMEOUT_MS);
  try {
    const outcome = await Promise.all([
      collect(child.stdout),
      collect(child.stderr),
      once(child, "exit"),
    ]) as [string, string, [number | null, string]];
    const raw = outcome[0].trim();
    const code = outcome[2][0];
    if (raw.length === 0) {
      throw new Error(`child printed nothing (exit ${String(code)}): ${outcome[1]}`);
    }
    let parsed: ChildResultPayload;
    try {
      parsed = JSON.parse(raw) as ChildResultPayload;
    } catch {
      throw new Error(`child printed non-JSON: ${raw} / ${outcome[1]}`);
    }
    if (parsed.error !== undefined) {
      throw new Error(`child failed beyond the resolver: ${JSON.stringify(parsed.error)}`);
    }
    return { code, ...parsed };
  } finally {
    clearTimeout(killTimer);
  }
}

interface ChildResultPayload {
  readonly error?: string;
  readonly resolved?: string;
  readonly protocolVersion?: number;
  readonly failure?: RuntimePathFailure;
}

/**
 * A child process only ever sees the fixture's own environment plus PATH:
 * inherited host variables such as XDG_RUNTIME_DIR could defeat both the
 * sandbox and the resolver contract.
 */
function childEnv(environment: FixtureEnvironment): NodeJS.ProcessEnv {
  return {
    PATH: process.env["PATH"] ?? "/usr/bin:/bin",
    HOME: environment.HOME ?? "/home/operator",
    XDG_RUNTIME_DIR: environment.XDG_RUNTIME_DIR,
    XDG_DATA_HOME: environment.XDG_DATA_HOME,
    XDG_STATE_HOME: environment.XDG_STATE_HOME,
    XDG_CACHE_HOME: environment.XDG_CACHE_HOME,
    XDG_CONFIG_HOME: environment.XDG_CONFIG_HOME,
  };
}

/** A private, short sandbox root under the testkit's temporary parent. */
function placeholder(): string {
  const root = createFixtureRootSync("pk-rtp-");
  placeholderRoots.push(root);
  return root;
}

/** Drains a child stream to a string; the child is killed by the timer above. */
async function collect(stream: Readable | null): Promise<string> {
  if (stream === null) {
    return "";
  }
  const chunks: Buffer[] = [];
  for await (const chunk of stream) {
    chunks.push(Buffer.from(chunk as Buffer));
  }
  return Buffer.concat(chunks).toString("utf8");
}
