// End-to-end contract of the SDK release assets: real tarballs from
// `pack-release.ts`, a real HTTP server standing in for the GitHub release,
// and a real `bun install` of a consumer project that depends on the tarballs
// by URL. The consumer's registry points at a closed port, so any attempt to
// resolve `@pohunek/*` (or anything else) from a registry fails the install.
//
// The packed artifact is checked under every runtime and resolver the docs
// promise: Bun, the Node binary named by POHUNEK_TEST_NODE_BIN, and
// `tsc --noEmit` with both `moduleResolution: "bundler"` and `"nodenext"`.
// `@pohunek/testkit/bun-relay` is Bun-only and is exercised under Bun alone.

import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { packRelease, type PackedPackage } from "../pack-release";

const execFileAsync = promisify(execFile);
const TEST_VERSION = "0.0.0-test";
const SOURCE_DATE_EPOCH = 1_700_000_000;
const INSTALL_TIMEOUT_MS = 120_000;
const PACK_TIMEOUT_MS = 30_000;
const BUN_EXECUTABLE = process.execPath;
const REPOSITORY_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "..");
const TSC_ENTRY = join(REPOSITORY_ROOT, "node_modules", "typescript", "bin", "tsc");
const NODE_TYPES_ROOT = join(REPOSITORY_ROOT, "node_modules", "@types");
// The Node binary the runtime assertions run under. The CI and release SDK jobs
// run this file once per Node major with it set; a run without it skips them.
const NODE_BIN_ENV = "POHUNEK_TEST_NODE_BIN";
const NODE_BIN = process.env[NODE_BIN_ENV];
// Oldest Node major the docs promise. The SDK reads `globalThis.crypto`, which
// Node exposes unflagged from 19 on; Node 18 fails at module load.
const MIN_NODE_MAJOR = 20;
// First Node major with the WHATWG `WebSocket` global.
const WEBSOCKET_NODE_MAJOR = 22;
const NODE_TIMEOUT_MS = 60_000;
const TSC_TIMEOUT_MS = 120_000;
const SKIP_NODE_MESSAGE = `${NODE_BIN_ENV} is unset: skipping the Node runtime assertions (CI sets it)`;
if (NODE_BIN === undefined) {
  console.warn(SKIP_NODE_MESSAGE);
}
// Port 9 (discard) is never served by a registry; connections are refused.
const DEAD_REGISTRY = "http://127.0.0.1:9/";

const CONSUMER_SCRIPT = `
import { Client } from "@pohunek/sdk";
import * as browser from "@pohunek/sdk/browser";
import * as protocol from "@pohunek/protocol";
import * as relay from "@pohunek/protocol/relay";
import * as testkit from "@pohunek/testkit";
import fixture from "@pohunek/protocol/fixtures/session-info-minimal.json" with { type: "json" };

const surfaces = { sdk: Client, browser, protocol, relay, testkit };
for (const [name, surface] of Object.entries(surfaces)) {
  if (surface === undefined || (typeof surface === "object" && Object.keys(surface).length === 0)) {
    throw new Error("empty import surface: " + name);
  }
}
if (typeof browser.Client !== "function" || browser.Client !== Client) {
  throw new Error("sdk root and browser entries disagree on Client");
}
// The browser subpath exposes the browser transports only: no node-only socket
// helpers leak into it, and the request ids need no crypto.randomUUID.
if (typeof browser.WsTransport !== "function" || typeof browser.nextRequestId !== "function") {
  throw new Error("browser entry does not export the browser transports");
}
if ("SocketTransport" in browser || "connectLocal" in browser) {
  throw new Error("browser entry exports a node-only socket API");
}
const randomUUIDDescriptor = Object.getOwnPropertyDescriptor(globalThis.crypto, "randomUUID");
Object.defineProperty(globalThis.crypto, "randomUUID", { configurable: true, value: undefined });
try {
  const firstId = browser.nextRequestId("browser");
  const secondId = browser.nextRequestId("browser");
  if (!firstId.startsWith("sdk-browser-") || !firstId.endsWith("-0") || secondId !== firstId.slice(0, -2) + "-1") {
    throw new Error("nextRequestId produced unexpected shapes without crypto.randomUUID: " + firstId + " / " + secondId);
  }
  const defaults = Client.defaultOptions();
  if (defaults.connectTimeoutMs !== 5000 || defaults.requestTimeoutMs !== 5000) {
    throw new Error("connect options do not default to 5000ms");
  }
} finally {
  if (randomUUIDDescriptor === undefined) {
    Reflect.deleteProperty(globalThis.crypto, "randomUUID");
  } else {
    Object.defineProperty(globalThis.crypto, "randomUUID", randomUUIDDescriptor);
  }
}
if (typeof testkit.startFixtureDaemon !== "function") {
  throw new Error("testkit does not export startFixtureDaemon");
}
if (typeof fixture !== "object") {
  throw new Error("fixture did not load");
}
if ("startTestRelay" in testkit) {
  throw new Error("the testkit root entry exposes the Bun-only relay");
}
console.log("consumer-ok");
`;

// Runs under Bun against the installed tarball: a real WebSocket through the
// relay reaches a real Unix-socket daemon stand-in that echoes every byte, and
// an unknown route is a 404.
const RELAY_CONSUMER_SCRIPT = `
import { createServer } from "node:net";
import { join } from "node:path";
import { startTestRelay } from "@pohunek/testkit/bun-relay";

const [scratchDir] = process.argv.slice(2);
const socketPath = join(scratchDir, "echo.sock");
const daemon = createServer((socket) => {
  socket.on("data", (chunk) => socket.write(chunk));
});
await new Promise((resolve, reject) => {
  daemon.once("error", reject);
  daemon.listen(socketPath, resolve);
});
const relay = await startTestRelay({
  bindHost: "127.0.0.1",
  port: 0,
  targets: new Map([["local", { kind: "unix", socketPath }]]),
});
try {
  const missing = await fetch(relay.url + "/not-a-relay-route");
  if (missing.status !== 404) {
    throw new Error("unknown route returned " + missing.status);
  }
  const plain = await fetch(relay.url + "/daemon/local/control");
  if (plain.status !== 426) {
    throw new Error("control route without an upgrade returned " + plain.status);
  }
  // The relay is owner-only transport: a non-loopback bind must be refused
  // before any socket is opened, with an error naming the restriction.
  let bindError;
  for (const bindHost of ["0.0.0.0", "::", "192.168.1.10", "localhost"]) {
    bindError = undefined;
    try {
      const relay = await startTestRelay({ bindHost, port: 0, targets: new Map() });
      await relay.close();
      throw new Error("non-loopback bind was accepted: " + bindHost);
    } catch (error) {
      bindError = error;
    }
    if (!(bindError instanceof Error) || !bindError.message.includes("only binds to loopback")) {
      throw new Error("non-loopback bind did not fail with the loopback refusal: " + String(bindError));
    }
  }
  const socket = new WebSocket("ws://127.0.0.1:" + relay.port + "/daemon/local/control");
  const echoed = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("no echo from the relay")), 10000);
    socket.addEventListener("open", () => socket.send("hello"));
    socket.addEventListener("message", (event) => {
      clearTimeout(timer);
      resolve(event.data);
    });
    socket.addEventListener("error", () => {
      clearTimeout(timer);
      reject(new Error("relay websocket failed"));
    });
  });
  if (echoed !== "hello") {
    throw new Error("relay echoed " + echoed);
  }
  socket.close();
} finally {
  await relay.close();
  await new Promise((resolve) => daemon.close(resolve));
}
console.log("relay-consumer-ok");
`;

// Runs under plain Node: no TypeScript loader, no bundler, no Bun globals.
const NODE_CONSUMER_SCRIPT = `
import { join } from "node:path";
import {
  Client,
  ClientError,
  connectLocal,
  nextRequestId,
} from "@pohunek/sdk";
import * as browser from "@pohunek/sdk/browser";
import { MAX_CONTROL_LINE_BYTES, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION } from "@pohunek/protocol";
import * as relay from "@pohunek/protocol/relay";
import * as testkit from "@pohunek/testkit";
import fixture from "@pohunek/protocol/fixtures/session-info-minimal.json" with { type: "json" };

const [scratchDir] = process.argv.slice(2);
const major = Number(process.versions.node.split(".")[0]);
const websocketMajor = ${WEBSOCKET_NODE_MAJOR};

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

assert(browser.Client === Client, "sdk root and browser entries disagree on Client");
assert(typeof browser.connectLocal === "undefined", "browser entry exports a node-only helper");
assert(nextRequestId("probe").startsWith("sdk-probe-"), "nextRequestId has an unexpected shape");
assert(Client.defaultOptions().requestTimeoutMs > 0, "default request timeout is not positive");
assert(Client.defaultOptions().connectTimeoutMs === Client.defaultOptions().requestTimeoutMs, "connect and request timeouts do not default to the same value");
assert(Number.isInteger(PROTOCOL_VERSION) && MIN_PROTOCOL_VERSION <= PROTOCOL_VERSION, "protocol versions are inconsistent");
assert(MAX_CONTROL_LINE_BYTES > 0, "protocol line limit is not positive");
assert(typeof relay.API_VERSION === "number", "relay constants did not load");
assert(typeof testkit.startFixtureDaemon === "function", "testkit does not export startFixtureDaemon");
assert(typeof fixture === "object" && fixture !== null, "fixture did not load");
assert(!("startTestRelay" in testkit), "the testkit root entry exposes the Bun-only relay");

// The Bun-only subpath loads under Node but refuses to start, naming itself.
const bunRelay = await import("@pohunek/testkit/bun-relay");
assert(typeof bunRelay.startTestRelay === "function", "bun-relay does not export startTestRelay");
let relayError;
try {
  await bunRelay.startTestRelay({ bindHost: "127.0.0.1", port: 0, targets: new Map() });
} catch (error) {
  relayError = error;
}
assert(relayError instanceof Error && relayError.message.includes("@pohunek/testkit/bun-relay is Bun-only"), "bun-relay did not fail fast under Node");

// node:net transport: dialling an absent socket fails with a structured client error.
let socketError;
try {
  await connectLocal(join(scratchDir, "absent.sock"), { connectTimeoutMs: 2000 });
} catch (error) {
  socketError = error;
}
assert(socketError instanceof ClientError && socketError.kind === "daemonUnreachable", "socket transport did not reject with daemonUnreachable");

if (major >= websocketMajor) {
  // WHATWG WebSocket global: a refused relay connection is a structured client error.
  let wsError;
  try {
    await Client.connectWs("ws://127.0.0.1:9", "probe", { connectTimeoutMs: 2000 });
  } catch (error) {
    wsError = error;
  }
  assert(wsError instanceof ClientError, "WebSocket transport did not reject with a ClientError");
}
console.log("node-consumer-ok " + process.versions.node);
`;

// Type-checked, never executed: proves the declarations resolve and infer.
const TS_CONSUMER_SOURCE = `
import { Client, connectLocal, nextRequestId, type Request } from "@pohunek/sdk";
import { Client as BrowserClient } from "@pohunek/sdk/browser";
import { PROTOCOL_VERSION, type SessionInfo } from "@pohunek/protocol";
import { API_VERSION } from "@pohunek/protocol/relay";
import { startFixtureDaemon } from "@pohunek/testkit";
import { startTestRelay, type TestRelayHandle } from "@pohunek/testkit/bun-relay";

export const version: number = PROTOCOL_VERSION;
export const relayVersion: number = API_VERSION;
export const requestId: string = nextRequestId("probe");
export const sameClient: typeof Client = BrowserClient;
export const dial: typeof connectLocal = connectLocal;
export const start: typeof startFixtureDaemon = startFixtureDaemon;
export const relayStart: typeof startTestRelay = startTestRelay;
export type RelayHandle = TestRelayHandle;
export type RequestShape = Request;
export async function listSessions(client: Client): Promise<SessionInfo[]> {
  const sessions = await client.call("session.list", {});
  const first: SessionInfo | undefined = sessions[0];
  void first;
  return sessions;
}
`;

const TS_RESOLUTIONS = [
  { name: "bundler", module: "esnext", moduleResolution: "bundler" },
  { name: "nodenext", module: "nodenext", moduleResolution: "nodenext" },
] as const;

let workDir = "";
let outDir = "";
let packed: readonly PackedPackage[] = [];
let server: ReturnType<typeof Bun.serve> | undefined;
let baseUrl = "";

async function expectRejection(operation: Promise<unknown>, fragment?: string): Promise<void> {
  let message: string | undefined;
  try {
    await operation;
  } catch (error) {
    message = error instanceof Error ? error.message : String(error);
  }
  expect(message).toBeDefined();
  if (fragment !== undefined) {
    expect(message).toContain(fragment);
  }
}

async function tarList(tarball: string): Promise<string[]> {
  const { stdout } = await execFileAsync("tar", ["-tzf", tarball]);
  return stdout.split("\n").filter((line) => line !== "");
}

async function runBun(args: string[], cwd: string): Promise<string> {
  const { stdout } = await execFileAsync(BUN_EXECUTABLE, args, {
    cwd,
    env: { ...process.env, BUN_INSTALL_CACHE_DIR: join(workDir, "bun-cache") },
  });
  return stdout;
}

let consumerInstall: Promise<string> | undefined;

// Installs the packed tarballs by URL into a throwaway consumer once; every
// consumer-facing test shares it.
function installedConsumer(): Promise<string> {
  consumerInstall ??= (async () => {
    const consumer = join(workDir, "consumer");
    await mkdir(consumer, { recursive: true });
    await writeFile(
      join(consumer, "package.json"),
      JSON.stringify(
        {
          name: "pack-contract-consumer",
          private: true,
          type: "module",
          dependencies: {
            "@pohunek/sdk": `${baseUrl}/pohunek-ts-sdk-${TEST_VERSION}.tgz`,
            "@pohunek/testkit": `${baseUrl}/pohunek-ts-testkit-${TEST_VERSION}.tgz`,
          },
        },
        null,
        2,
      ),
    );
    await writeFile(join(consumer, "bunfig.toml"), `[install]\nregistry = "${DEAD_REGISTRY}"\n`);
    await writeFile(join(consumer, "consumer.ts"), CONSUMER_SCRIPT);
    await writeFile(join(consumer, "node-consumer.mjs"), NODE_CONSUMER_SCRIPT);
    await writeFile(join(consumer, "ts-consumer.ts"), TS_CONSUMER_SOURCE);
    await writeFile(join(consumer, "relay-consumer.ts"), RELAY_CONSUMER_SCRIPT);
    await runBun(["install"], consumer);
    return consumer;
  })();
  return consumerInstall;
}

// Relative `./x.js` specifiers a built module imports, and every bare or
// `node:` specifier it names.
function moduleSpecifiers(source: string): string[] {
  return [...source.matchAll(/(?:^|\n)\s*(?:import|export)\b[^;]*?\sfrom\s*"([^"]+)"|(?:^|\n)\s*import\s*"([^"]+)"/gu)].map(
    (match) => match[1] ?? match[2] ?? "",
  );
}

// Every specifier reachable from `entry` through relative imports.
async function reachableSpecifiers(packageRoot: string, entry: string): Promise<Set<string>> {
  const seen = new Set<string>();
  const specifiers = new Set<string>();
  const queue = [entry];
  while (queue.length > 0) {
    const file = queue.pop() as string;
    if (seen.has(file)) {
      continue;
    }
    seen.add(file);
    for (const specifier of moduleSpecifiers(await readFile(join(packageRoot, file), "utf8"))) {
      specifiers.add(specifier);
      if (specifier.startsWith(".")) {
        queue.push(posix.normalize(posix.join(posix.dirname(file), specifier)));
      }
    }
  }
  return specifiers;
}

// Packaging builds three real release assets, so it shares the bounded
// subprocess budget instead of Bun's short default hook timeout.
beforeAll(async () => {
  workDir = await mkdtemp(join(tmpdir(), "pohunek-pack-contract-"));
  outDir = join(workDir, "assets");
  // Serves the asset whose name is the last URL path segment, like a GitHub
  // release download URL (`.../releases/download/<tag>/<asset>`).
  server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(request) {
      const name = new URL(request.url).pathname.split("/").pop() ?? "";
      const file = Bun.file(join(outDir, name));
      if (!/^pohunek-ts-[a-z]+-[0-9A-Za-z.-]+\.tgz$/u.test(name) || !(await file.exists())) {
        return new Response("not found", { status: 404 });
      }
      return new Response(file);
    },
  });
  baseUrl = `http://127.0.0.1:${server.port}/releases/download/v${TEST_VERSION}`;
  packed = await packRelease({
    version: TEST_VERSION,
    baseUrl,
    outDir,
    sourceDateEpoch: SOURCE_DATE_EPOCH,
  });
}, INSTALL_TIMEOUT_MS);

afterAll(async () => {
  await server?.stop(true);
  await rm(workDir, { recursive: true, force: true });
});

describe("SDK release pack contract", () => {
  test("writes one tarball and checksum per package with the documented names", async () => {
    const names = (await readdir(outDir)).sort();
    expect(names).toEqual(
      [
        `pohunek-ts-protocol-${TEST_VERSION}.tgz`,
        `pohunek-ts-protocol-${TEST_VERSION}.tgz.sha256`,
        `pohunek-ts-sdk-${TEST_VERSION}.tgz`,
        `pohunek-ts-sdk-${TEST_VERSION}.tgz.sha256`,
        `pohunek-ts-testkit-${TEST_VERSION}.tgz`,
        `pohunek-ts-testkit-${TEST_VERSION}.tgz.sha256`,
      ].sort(),
    );
  });

  test("checksum files verify against the tarball bytes", async () => {
    for (const item of packed) {
      const sidecar = await readFile(item.checksum, "utf8");
      const digest = createHash("sha256").update(await readFile(item.tarball)).digest("hex");
      expect(sidecar).toBe(`${digest}  ${item.tarball.split("/").pop()}\n`);
    }
  });

  test("tarballs hold compiled ESM, declarations and the licence but no sources, tests or build state", async () => {
    for (const item of packed) {
      const listing = await tarList(item.tarball);
      expect(listing).toContain("package/package.json");
      expect(listing).toContain("package/LICENSE");
      expect(listing.some((entry) => /^package\/dist\/.+\.js$/u.test(entry))).toBe(true);
      expect(listing.some((entry) => /^package\/types\/.+\.d\.ts$/u.test(entry))).toBe(true);
      for (const entry of listing) {
        expect(entry.startsWith("package/")).toBe(true);
        expect(/(^|\/)(src|test|dist-types|node_modules)\//u.test(entry)).toBe(false);
        expect(/\.(test\.ts|tsbuildinfo|map)$/u.test(entry)).toBe(false);
        // The only TypeScript files shipped are declarations.
        expect(entry.endsWith(".ts") && !entry.endsWith(".d.ts")).toBe(false);
      }
    }
    const protocolListing = await tarList(packed[0]!.tarball);
    expect(protocolListing).toContain("package/fixtures/session-info-minimal.json");
  });

  test("packed manifests are versioned, public and point inner dependencies at release URLs", async () => {
    for (const item of packed) {
      const dir = join(workDir, `inspect-${item.name.replace("/", "-")}`);
      await mkdir(dir, { recursive: true });
      await execFileAsync("tar", ["-xzf", item.tarball, "-C", dir]);
      const manifest = JSON.parse(await readFile(join(dir, "package", "package.json"), "utf8")) as Record<string, unknown>;
      expect(manifest["version"]).toBe(TEST_VERSION);
      expect(manifest["private"]).toBeUndefined();
      expect(manifest["devDependencies"]).toBeUndefined();
      expect(manifest["scripts"]).toBeUndefined();
      expect(JSON.stringify(manifest["exports"])).not.toContain("/src/");
      expect(JSON.stringify(manifest["exports"])).toContain("./dist/");
      expect(JSON.stringify(manifest["exports"])).toContain("./types/");
      for (const [subpath, conditions] of Object.entries(manifest["exports"] as Record<string, Record<string, string>>)) {
        if (subpath.startsWith("./fixtures/")) {
          continue;
        }
        expect(Object.keys(conditions)).toEqual(["types", "import", "default"]);
      }
      const dependencies = (manifest["dependencies"] ?? {}) as Record<string, string>;
      for (const [name, range] of Object.entries(dependencies)) {
        expect(name).toBe("@pohunek/protocol");
        expect(range).toBe(`${baseUrl}/pohunek-ts-protocol-${TEST_VERSION}.tgz`);
      }
    }
  });

  test("packing twice yields byte-identical tarballs", async () => {
    const second = await packRelease({
      version: TEST_VERSION,
      baseUrl,
      outDir: join(workDir, "assets-again"),
      sourceDateEpoch: SOURCE_DATE_EPOCH,
    });
    for (const [index, item] of second.entries()) {
      expect(item.sha256).toBe(packed[index]!.sha256);
    }
  }, PACK_TIMEOUT_MS);

  test("the release script refuses invalid inputs on its real command line", async () => {
    // The script runs exactly as the Release workflow runs it, with
    // SOURCE_DATE_EPOCH and its flags on the CLI; each hostile input must fail
    // the process with the refusing message, never a default.
    const script = join(REPOSITORY_ROOT, "sdk", "ts", "scripts", "pack-release.ts");
    const cases: readonly [string, readonly string[]][] = [
      ["version", ["--version", "v1.2.3", "--base-url", baseUrl, "--out", join(workDir, "rejected")]],
      ["base URL", ["--version", TEST_VERSION, "--base-url", "not a url", "--out", join(workDir, "rejected")]],
      ["http or https", ["--version", TEST_VERSION, "--base-url", "ftp://example.com/x", "--out", join(workDir, "rejected")]],
      ["query", ["--version", TEST_VERSION, "--base-url", `${baseUrl}?token=1`, "--out", join(workDir, "rejected")]],
      ["output directory", ["--version", TEST_VERSION, "--base-url", baseUrl, "--out", ""]],
    ];
    for (const [fragment, flags] of cases) {
      const rejected = await execFileAsync(BUN_EXECUTABLE, [script, ...flags], {
        cwd: REPOSITORY_ROOT,
        env: { ...process.env, SOURCE_DATE_EPOCH: String(SOURCE_DATE_EPOCH) },
      }).then(() => undefined, (error: { stderr?: string }) => error as unknown);
      expect(rejected).toBeDefined();
      const stderr = (rejected as { stderr?: string }).stderr ?? "";
      expect(stderr).toContain("pack-release:");
      expect(stderr).toContain(fragment);
    }
    const epochCase = await execFileAsync(BUN_EXECUTABLE, [
      script,
      "--version", TEST_VERSION,
      "--base-url", baseUrl,
      "--out", join(workDir, "rejected"),
    ], {
      cwd: REPOSITORY_ROOT,
      env: { ...process.env, SOURCE_DATE_EPOCH: "-1" },
    }).then(() => undefined, (error: { stderr?: string }) => error as unknown);
    expect(epochCase).toBeDefined();
    expect((epochCase as { stderr?: string }).stderr ?? "").toContain("SOURCE_DATE_EPOCH");
  });

  test("a consumer installs @pohunek/sdk and @pohunek/testkit by URL and imports every entry point", async () => {
    const consumer = await installedConsumer();

    // (a) one protocol copy, resolved from the served tarball.
    const scopeDir = join(consumer, "node_modules", "@pohunek");
    expect((await readdir(scopeDir)).sort()).toEqual(["protocol", "sdk", "testkit"]);
    for (const nested of ["sdk", "testkit"]) {
      await expectRejection(stat(join(scopeDir, nested, "node_modules")));
    }
    const lockfile = await readFile(join(consumer, "bun.lock"), "utf8");
    const protocolUrl = `${baseUrl}/pohunek-ts-protocol-${TEST_VERSION}.tgz`;
    expect(lockfile).toContain(protocolUrl);
    expect(lockfile).toContain("sha512-");
    expect(lockfile).not.toContain(DEAD_REGISTRY);
    const installedProtocol = JSON.parse(await readFile(join(scopeDir, "protocol", "package.json"), "utf8")) as Record<string, unknown>;
    expect(installedProtocol["version"]).toBe(TEST_VERSION);

    // (b) every documented entry point imports under Bun.
    const output = await runBun(["run", "consumer.ts"], consumer);
    expect(output).toContain("consumer-ok");
  }, INSTALL_TIMEOUT_MS);

  test("every entry point imports and runs under Node", async () => {
    if (NODE_BIN === undefined) {
      console.warn(SKIP_NODE_MESSAGE);
      return;
    }
    const consumer = await installedConsumer();
    const { stdout: versionText } = await execFileAsync(NODE_BIN, ["--version"]);
    const major = Number(/^v(\d+)\./u.exec(versionText.trim())?.[1]);
    expect(major).toBeGreaterThanOrEqual(MIN_NODE_MAJOR);
    const scratch = join(workDir, "node-scratch");
    await mkdir(scratch, { recursive: true });
    const { stdout } = await execFileAsync(NODE_BIN, ["node-consumer.mjs", scratch], {
      cwd: consumer,
      timeout: NODE_TIMEOUT_MS,
    });
    expect(stdout).toContain(`node-consumer-ok ${versionText.trim().slice(1)}`);
  }, INSTALL_TIMEOUT_MS + NODE_TIMEOUT_MS);

  for (const resolution of TS_RESOLUTIONS) {
    test(`declarations type-check under moduleResolution "${resolution.name}"`, async () => {
      const consumer = await installedConsumer();
      const config = join(consumer, `tsconfig.${resolution.name}.json`);
      await writeFile(
        config,
        JSON.stringify({
          compilerOptions: {
            strict: true,
            noEmit: true,
            target: "ES2022",
            lib: ["ES2022", "DOM"],
            module: resolution.module,
            moduleResolution: resolution.moduleResolution,
            // Declarations are checked, not skipped: a broken `.d.ts` must fail here.
            skipLibCheck: false,
            types: ["node"],
            typeRoots: [NODE_TYPES_ROOT],
          },
          files: ["ts-consumer.ts"],
        }),
      );
      await runBun([TSC_ENTRY, "-p", config], consumer);
    }, TSC_TIMEOUT_MS);
  }

  test("the installed @pohunek/testkit/bun-relay subpath tunnels a real WebSocket to a real daemon socket under Bun", async () => {
    const consumer = await installedConsumer();
    const scratch = join(workDir, "relay-scratch");
    await mkdir(scratch, { recursive: true });
    const output = await runBun(["run", "relay-consumer.ts", scratch], consumer);
    expect(output).toContain("relay-consumer-ok");
  }, INSTALL_TIMEOUT_MS);

  test("the browser entry reaches no node: module while the root entry does", async () => {
    const consumer = await installedConsumer();
    const sdkRoot = join(consumer, "node_modules", "@pohunek", "sdk");
    const browserSpecifiers = await reachableSpecifiers(sdkRoot, "dist/index.browser.js");
    expect([...browserSpecifiers].filter((specifier) => specifier.startsWith("node:"))).toEqual([]);
    const rootSpecifiers = await reachableSpecifiers(sdkRoot, "dist/index.js");
    expect(rootSpecifiers.has("node:net")).toBe(true);
  }, INSTALL_TIMEOUT_MS);
});
