import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostRecord } from "@pohunek/protocol";
import {
  BackendStartupError,
  startBackend,
  startHostsPipeline,
  type BackendHandle,
  type BackendHostEntry,
  type BackendLogger,
} from "@pohunek/backend";
import { Client, attachRawWs, type RawStream } from "@pohunek/sdk";
import {
  DEFAULT_PTY_READY_BYTES,
  startFixtureDaemon,
  type FixtureDaemonHandle,
} from "@pohunek/testkit";

const LOOPBACK_HOST = "127.0.0.1";
const LOCAL_DAEMON_VERSION = "2.0.0-local-test";
const PEER_DAEMON_VERSION = "2.0.0-peer-test";
const PEER_HOST = "peer-one";
const TEST_COLS = 80;
const TEST_ROWS = 24;
const DISCOVER_INTERVAL_SECONDS = 0.02;
const POLL_INTERVAL_MILLISECONDS = 10;
const POLL_TIMEOUT_MILLISECONDS = 2_000;
const BINARY_PAYLOAD = Uint8Array.of(0x00, 0xff, 0x80, 0x61, 0xc3, 0x28);
const INDEX_CONTENT = "<!doctype html><title>Pohunek backend test</title>";
const ASSET_CONTENT = "backend-test-asset";

const silentLogger: BackendLogger = {
  log(): void {},
};

interface BackendFixture {
  readonly root: string;
  readonly local: FixtureDaemonHandle;
  readonly peer: FixtureDaemonHandle;
  readonly backend: BackendHandle;
  close(): Promise<void>;
}

describe("@pohunek/backend", () => {
  test("discovers and tunnels to local and peer daemons on one origin", async () => {
    const fixture = await startBackendFixture();
    try {
      const hostsResponse = await fetch(`${fixture.backend.url}/api/hosts`);
      expect(hostsResponse.status).toBe(200);
      expect(hostsResponse.headers.get("content-type")).toBe("application/json; charset=utf-8");
      expect(await readHosts(hostsResponse)).toEqual([
        {
          host: "local",
          reachability: "reachable_daemon",
          daemon_version: LOCAL_DAEMON_VERSION,
          protocol_version: PROTOCOL_VERSION,
        },
        {
          host: PEER_HOST,
          reachability: "reachable_daemon",
          daemon_version: PEER_DAEMON_VERSION,
        },
      ]);

      const localClient = await Client.connectWs(fixture.backend.url, "local");
      const peerClient = await Client.connectWs(fixture.backend.url, PEER_HOST);
      try {
        const localHealth = await localClient.call("daemon.health", null);
        const peerHealth = await peerClient.call("daemon.health", null);
        expect(localHealth.daemon_version).toBe(LOCAL_DAEMON_VERSION);
        expect(peerHealth.daemon_version).toBe(PEER_DAEMON_VERSION);

        const created = await peerClient.call("session.new", {
          agent: "shell",
          cols: TEST_COLS,
          rows: TEST_ROWS,
        });
        const attach = await peerClient.call("session.attach", { session_id: created.id });
        const raw = await attachRawWs(fixture.backend.url, PEER_HOST, attach.stream_id);
        await expectRoundTrip(raw, BINARY_PAYLOAD);

        const stopped = await peerClient.call("session.stop", created.id);
        expect(stopped.stopped).toBe(true);
      } finally {
        await localClient.close();
        await peerClient.close();
      }
    } finally {
      await fixture.close();
    }
  });

  test("periodic refresh drops a stopped peer route and preserves unreachable history", async () => {
    const fixture = await startBackendFixture();
    try {
      await fixture.peer.stopAbruptly();
      fixture.local.scenario.setDiscoveredHosts([]);

      await waitFor(async (): Promise<boolean> => {
        const hosts = await readHosts(await fetch(`${fixture.backend.url}/api/hosts`));
        return hosts.some(
          (entry) => entry.host === PEER_HOST && entry.reachability === "unreachable",
        );
      });

      const hosts = await readHosts(await fetch(`${fixture.backend.url}/api/hosts`));
      expect(hosts).toEqual([
        {
          host: "local",
          reachability: "reachable_daemon",
          daemon_version: LOCAL_DAEMON_VERSION,
          protocol_version: PROTOCOL_VERSION,
        },
        { host: PEER_HOST, reachability: "unreachable" },
      ]);

      const removedRoute = await fetch(`${fixture.backend.url}/daemon/${PEER_HOST}/control`);
      expect(removedRoute.status).toBe(404);
    } finally {
      await fixture.close();
    }
  });

  test("serves assets and falls back to index.html for client-side routes", async () => {
    const fixture = await startBackendFixture();
    try {
      const asset = await fetch(`${fixture.backend.url}/app.js`);
      expect(asset.status).toBe(200);
      expect(asset.headers.get("content-type")).toBe("text/javascript; charset=utf-8");
      expect(await asset.text()).toBe(ASSET_CONTENT);

      const clientRoute = await fetch(`${fixture.backend.url}/sessions/s-test`);
      expect(clientRoute.status).toBe(200);
      expect(await clientRoute.text()).toBe(INDEX_CONTENT);

      const missingAsset = await fetch(`${fixture.backend.url}/missing.css`);
      expect(missingAsset.status).toBe(404);
    } finally {
      await fixture.close();
    }
  });

  test("startup fails through a typed actionable path without a local daemon", async () => {
    const root = await mkdtemp(join(tmpdir(), "pohunek-backend-missing-"));
    const socketPath = join(root, "missing.sock");
    try {
      let thrown: unknown;
      try {
        await startHostsPipeline({
          daemonSocketPath: socketPath,
          remotePort: 18_722,
          discoverIntervalSeconds: 1,
          logger: silentLogger,
        });
      } catch (error: unknown) {
        thrown = error;
      }

      expect(thrown).toBeInstanceOf(BackendStartupError);
      const startupError = thrown as BackendStartupError;
      expect(startupError.kind).toBe("localDaemonUnavailable");
      expect(startupError.socketPath).toBe(socketPath);
      expect(startupError.cause === undefined).toBe(false);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

async function startBackendFixture(): Promise<BackendFixture> {
  const root = await mkdtemp(join(tmpdir(), "pohunek-backend-test-"));
  const assets = join(root, "assets");
  await mkdir(assets);
  await writeFile(join(assets, "index.html"), INDEX_CONTENT);
  await writeFile(join(assets, "app.js"), ASSET_CONTENT);

  const peer = await startFixtureDaemon({
    listen: { tcp: { host: LOOPBACK_HOST, port: 0 } },
    daemonVersion: PEER_DAEMON_VERSION,
  });
  const peerAddress = peer.tcpAddress;
  if (peerAddress === undefined) {
    await peer.close();
    await rm(root, { recursive: true, force: true });
    throw new Error("peer fixture did not expose a TCP address");
  }

  const socketPath = join(root, "daemon.sock");
  const local = await startFixtureDaemon({
    listen: { unixSocketPath: socketPath },
    daemonVersion: LOCAL_DAEMON_VERSION,
    host: { discoveredHosts: [reachablePeerRecord()] },
  });

  let backend: BackendHandle;
  try {
    backend = await startBackend(
      {
        bindHost: LOOPBACK_HOST,
        port: 0,
        allowLoopbackBind: true,
        daemonSocketPath: socketPath,
        remotePort: peerAddress.port,
        discoverIntervalSeconds: DISCOVER_INTERVAL_SECONDS,
        staticAssetsDir: assets,
      },
      silentLogger,
    );
  } catch (error: unknown) {
    await local.close();
    await peer.close();
    await rm(root, { recursive: true, force: true });
    throw error;
  }

  return {
    root,
    local,
    peer,
    backend,
    close: async (): Promise<void> => {
      await backend.close();
      await local.close();
      await peer.close();
      await rm(root, { recursive: true, force: true });
    },
  };
}

function reachablePeerRecord(): HostRecord {
  return {
    name: PEER_HOST,
    fqdn: `${PEER_HOST}.test.invalid`,
    netbird_ip: LOOPBACK_HOST,
    classification: "reachable_daemon",
    daemon_version: PEER_DAEMON_VERSION,
  };
}

async function readHosts(response: Response): Promise<readonly BackendHostEntry[]> {
  return await response.json() as readonly BackendHostEntry[];
}

async function expectRoundTrip(raw: RawStream, payload: Uint8Array): Promise<void> {
  const writer = raw.writable.getWriter();
  const reader = raw.readable.getReader();
  try {
    const ready = await readExactly(reader, DEFAULT_PTY_READY_BYTES.byteLength);
    expect(Array.from(ready)).toEqual(Array.from(DEFAULT_PTY_READY_BYTES));
    await writer.write(payload);
    const actual = await readExactly(reader, payload.byteLength);
    expect(Array.from(actual)).toEqual(Array.from(payload));
  } finally {
    writer.releaseLock();
    reader.releaseLock();
    await raw.close();
  }
}

async function readExactly(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  expectedBytes: number,
): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (total < expectedBytes) {
    const next = await reader.read();
    if (next.done === true) {
      throw new Error(`raw stream closed after ${total} of ${expectedBytes} bytes`);
    }
    chunks.push(next.value);
    total += next.value.byteLength;
  }
  const output = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

async function waitFor(predicate: () => Promise<boolean>): Promise<void> {
  const deadline = Date.now() + POLL_TIMEOUT_MILLISECONDS;
  while (Date.now() < deadline) {
    if (await predicate()) {
      return;
    }
    await delay(POLL_INTERVAL_MILLISECONDS);
  }
  throw new Error("timed out waiting for backend discovery refresh");
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolveDelay) => {
    setTimeout(resolveDelay, milliseconds);
  });
}
