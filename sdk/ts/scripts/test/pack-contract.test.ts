// End-to-end contract of the SDK release assets: real tarballs from
// `pack-release.ts`, a real HTTP server standing in for the GitHub release,
// and a real `bun install` of a consumer project that depends on the tarballs
// by URL. The consumer's registry points at a closed port, so any attempt to
// resolve `@pohunek/*` (or anything else) from a registry fails the install.

import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { packRelease, type PackedPackage } from "../pack-release";

const execFileAsync = promisify(execFile);
const TEST_VERSION = "0.0.0-test";
const SOURCE_DATE_EPOCH = 1_700_000_000;
const INSTALL_TIMEOUT_MS = 120_000;
const BUN_EXECUTABLE = process.execPath;
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
if (typeof testkit.startFixtureDaemon !== "function") {
  throw new Error("testkit does not export startFixtureDaemon");
}
if (typeof fixture !== "object") {
  throw new Error("fixture did not load");
}
console.log("consumer-ok");
`;

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
});

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

  test("tarballs hold sources and the licence but no tests or build output", async () => {
    for (const item of packed) {
      const listing = await tarList(item.tarball);
      expect(listing).toContain("package/package.json");
      expect(listing).toContain("package/LICENSE");
      expect(listing.some((entry) => entry.startsWith("package/src/"))).toBe(true);
      for (const entry of listing) {
        expect(entry.startsWith("package/")).toBe(true);
        expect(/(^|\/)(test|dist-types|node_modules)\//u.test(entry)).toBe(false);
        expect(/\.(test\.ts|tsbuildinfo)$/u.test(entry)).toBe(false);
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
  });

  test("rejects invalid inputs instead of defaulting", async () => {
    const valid = { version: TEST_VERSION, baseUrl, outDir: join(workDir, "rejected"), sourceDateEpoch: SOURCE_DATE_EPOCH };
    await expectRejection(packRelease({ ...valid, version: "v1.2.3" }), "version");
    await expectRejection(packRelease({ ...valid, baseUrl: "not a url" }), "base URL");
    await expectRejection(packRelease({ ...valid, baseUrl: "ftp://example.com/x" }), "http or https");
    await expectRejection(packRelease({ ...valid, baseUrl: `${baseUrl}?token=1` }), "query");
    await expectRejection(packRelease({ ...valid, sourceDateEpoch: -1 }), "SOURCE_DATE_EPOCH");
    await expectRejection(packRelease({ ...valid, outDir: "" }), "output directory");
  });

  test("a consumer installs @pohunek/sdk and @pohunek/testkit by URL and imports every entry point", async () => {
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

    await runBun(["install"], consumer);

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
});
