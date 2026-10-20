import { constants } from "node:fs";
import { access, mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { expect, test } from "bun:test";
import {
  GIT_FIXTURE_SCRIPT,
  addDaemonContext,
  createFixtureRoot,
  daemonBinaryPath,
  startDaemonProcess,
  withResource,
  withTimeout,
  type DaemonContext,
} from "@pohunek/testkit";
import { ClientError, resolveHost } from "@pohunek/sdk";

const E2E_ENABLED = process.env["POHUNEK_E2E"] === "1";
const E2E_TIMEOUT_MS = 30_000;
const BUN_TEST_TIMEOUT_BACKSTOP_MARGIN_MS = 5_000;
const APP_DIR = "pohunek";
const SOCKET_NAME = "daemon.sock";
const OVERLAY_PORT = 38_472;

const SELF_FQDN = "self.netbird.test";
const SELF_IP = "100.64.0.1";
const PEER_FQDN = "peer-b.netbird.test";
const PEER_IP = "100.64.0.9";
const PEER_KEY = "peer-key";
// `PEER_KEY` as a `peer~` selector spells it: unpadded base64url.
const PEER_KEY_SELECTOR = "cGVlci1rZXk";

/** A `netbird status --json` of this machine and three peers, two of which share a short name. */
const NETBIRD_FIXTURE_SCRIPT = `#!/bin/sh
if [ "$1" != "status" ] || [ "$2" != "--json" ]; then
  exit 2
fi
printf '%s\\n' '{"fqdn":"${SELF_FQDN}","netbirdIp":"${SELF_IP}","daemonStatus":"Connected","peers":{"details":[{"fqdn":"${PEER_FQDN}","netbirdIp":"${PEER_IP}","publicKey":"${PEER_KEY}","status":"Connected"},{"fqdn":"twin.a.netbird.test","netbirdIp":"100.64.0.20","publicKey":"twin-a","status":"Connected"},{"fqdn":"twin.b.netbird.test","netbirdIp":"100.64.0.21","publicKey":"twin-b","status":"Connected"}]}}'
`;

interface SkippableTest {
  (name: string, fn: () => void | Promise<void>, timeout?: number): void;
  readonly skip: SkippableTest;
}

const skippableTest = test as SkippableTest;
const realDaemonTest: SkippableTest = E2E_ENABLED ? skippableTest : skippableTest.skip;

realDaemonTest(
  "a real daemon resolves host routes through its overlay view",
  async () => {
    await withTimeout(
      startDaemon().then((started) =>
        withResource(
          started,
          async (daemon) => {
            try {
              await resolveAgainst(daemon.socketPath);
            } catch (error: unknown) {
              throw addDaemonContext(error, daemon);
            }
          },
          (daemon) => daemon.stop(),
          "host resolution e2e and daemon teardown both failed",
        ),
      ),
      E2E_TIMEOUT_MS,
      `host resolution real-daemon e2e did not finish within ${E2E_TIMEOUT_MS}ms`,
    );
  },
  E2E_TIMEOUT_MS + BUN_TEST_TIMEOUT_BACKSTOP_MARGIN_MS,
);

async function resolveAgainst(socketPath: string): Promise<void> {
  expect(await resolveHost("local", socketPath)).toEqual({ target: "local" });
  expect(await resolveHost(SELF_FQDN, socketPath)).toEqual({ target: "local" });

  expect(await resolveHost("peer-b", socketPath)).toEqual({
    target: "remote",
    display: "peer-b",
    address: { host: PEER_IP, port: OVERLAY_PORT },
    pinnedRoute: `netbird:peer~${PEER_KEY_SELECTOR}@${OVERLAY_PORT}`,
    overlay: "netbird",
    peerId: PEER_KEY,
    fqdn: PEER_FQDN,
  });

  // The pinned route selects the same peer; an explicit port is kept.
  const pinned = await resolveHost(`netbird:peer~${PEER_KEY_SELECTOR}@19000`, socketPath);
  expect(pinned).toEqual({
    target: "remote",
    display: `netbird:peer~${PEER_KEY_SELECTOR}@19000`,
    address: { host: PEER_IP, port: 19_000 },
    pinnedRoute: `netbird:peer~${PEER_KEY_SELECTOR}@19000`,
    overlay: "netbird",
    peerId: PEER_KEY,
    fqdn: PEER_FQDN,
  });

  expect(await failureCode(resolveHost("twin", socketPath))).toBe("overlay_host_ambiguous");
  expect(await failureCode(resolveHost("nobody", socketPath))).toBe("host_unknown");
}

async function failureCode(promise: Promise<unknown>): Promise<string> {
  const error = await promise.then(
    () => undefined,
    (caught: unknown) => caught,
  );
  expect(error).toBeInstanceOf(ClientError);
  return (error as ClientError).code;
}

interface Harness extends DaemonContext {
  stop(): Promise<void>;
}

async function startDaemon(): Promise<Harness> {
  const tempRoot = await createFixtureRoot("pk-hr-");
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
  const workerBin = join(dirname(daemonBin), "pohunek-sessiond");
  await access(workerBin, constants.X_OK);
  const daemon = await startDaemonProcess({
    daemonBin,
    cwd: tempRoot,
    env: {
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
      POHUNEK_WORKER_LAUNCHER: "subprocess",
      POHUNEK_WORKER_BIN: workerBin,
      POHUNEK_REMOTE_PORT: String(OVERLAY_PORT),
    },
    socketPath: join(dirs.runtime, APP_DIR, SOCKET_NAME),
    removeRoots: () => rm(tempRoot, { recursive: true, force: true }),
  });
  return {
    tempRoot,
    socketPath: daemon.socketPath,
    stdout: () => daemon.stdout(),
    stderr: () => daemon.stderr(),
    stop: () => daemon.stop(),
  };
}
