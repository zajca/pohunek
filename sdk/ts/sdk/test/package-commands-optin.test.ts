import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type PackageInstallParams } from "@pohunek/protocol";
import { ClientError, connectTcp, type Client } from "@pohunek/sdk";
import {
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * `accept_commands` is sent only to a daemon that advertises
 * `package_install_commands_supported`; a declined consent is left off the
 * wire because an older daemon rejects the key.
 */

const HOST = "netbird:package-commands";
const SETTLE_MS = 100;
const ARCHIVE = "/srv/packages/fleet-state.tar.zst";

const open: Array<{ close(): Promise<void> }> = [];

afterEach(async () => {
  while (open.length > 0) {
    await open.pop()?.close();
  }
});

function keepOpen(line: (requestLine: string) => string): ScriptStep {
  return { kind: "gate", ready: Promise.resolve(), line };
}

function inspectStep(flags: Partial<HostCapabilities>): ScriptStep {
  return keepOpen((line) => okResponseLine(requestIdFromLine(line), {
    daemon_version: "0.0.0-test",
    protocol_version: PROTOCOL_VERSION,
    supported_agents: [],
    runtimes: [],
    git_available: true,
    ...flags,
  }));
}

function installedStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), {
      status: "installed",
      package: { id: "means.fleet-state", version: "1.0.0" },
      reloaded: false,
    }),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("the mock daemon needs a TCP endpoint");
  }
  const client = await connectTcp(HOST, { host: daemon.endpoint.host, port: daemon.endpoint.port });
  open.push(client);
  return client;
}

async function start(steps: ScriptStep[]): Promise<MockDaemon> {
  const daemon = await startTcpDaemon(steps);
  open.push(daemon);
  return daemon;
}

async function methods(daemon: MockDaemon, count: number): Promise<Array<Record<string, unknown>>> {
  const seen: Array<Record<string, unknown>> = [];
  for (let index = 0; index < count; index += 1) {
    seen.push(parseRequestLine(await daemon.nextRequest()));
  }
  return seen;
}

function install(extra: Partial<PackageInstallParams> = {}): PackageInstallParams {
  return { archive_path: ARCHIVE, trust: { kind: "explicit_digest", digest: `sha256:${"0".repeat(64)}` }, enable: true, select: true, dry_run: false, ...extra };
}

function healthStep(): ScriptStep {
  return keepOpen((line) => okResponseLine(requestIdFromLine(line), {
    status: "ok",
    daemon_version: "0.0.0-test",
    protocol_version: PROTOCOL_VERSION,
  }));
}

describe("accept_commands opt-in", () => {
  test("a daemon without the flag is refused before package.install is sent", async () => {
    const daemon = await start([inspectStep({})]);
    const client = await connect(daemon);
    const error = await client.call("package.install", install({ accept_commands: true }))
      .then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(error?.kind).toBe("packageInstallCommandsUnsupported");
    expect(error?.code).toBe("package_install_commands_unsupported");
    expect(error?.errorClass).toBe("daemon");
    expect((await methods(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
  });

  test("package.link is gated the same way", async () => {
    const daemon = await start([inspectStep({})]);
    const client = await connect(daemon);
    const error = await client.call("package.link", { directory: "/srv/p", dry_run: true, accept_commands: true })
      .then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(error?.kind).toBe("packageInstallCommandsUnsupported");
    expect((await methods(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
  });

  test("a daemon with the flag receives accept_commands", async () => {
    const daemon = await start([inspectStep({ package_install_commands_supported: true }), installedStep()]);
    const client = await connect(daemon);
    await client.call("package.install", install({ accept_commands: true }));
    const seen = await methods(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "package.install"]);
    expect((seen[1]?.["params"] as Record<string, unknown>)["accept_commands"]).toBe(true);
  });

  test("a declined consent is left off the wire without a capability check", async () => {
    for (const extra of [{}, { accept_commands: false }]) {
      const daemon = await start([healthStep(), installedStep()]);
      const client = await connect(daemon);
      await client.call("package.install", install(extra));
      const seen = await methods(daemon, 2);
      expect(seen.map((request) => request["method"])).toEqual(["daemon.health", "package.install"]);
      expect("accept_commands" in (seen[1]?.["params"] as Record<string, unknown>)).toBe(false);
      await daemon.expectNoRequest(SETTLE_MS);
    }
  });

  test("the flag check is remembered per connection", async () => {
    const answer = keepOpen((line) => okResponseLine(requestIdFromLine(line), {
      status: "installed",
      package: { id: "means.fleet-state", version: "1.0.0" },
      reloaded: false,
    }));
    const daemon = await start([
      inspectStep({ package_install_commands_supported: true }),
      answer,
      installedStep(),
    ]);
    const client = await connect(daemon);
    await client.call("package.install", install({ accept_commands: true }));
    await client.call("package.install", install({ accept_commands: true }));
    expect((await methods(daemon, 3)).map((entry) => entry["method"]))
      .toEqual(["host.inspect", "package.install", "package.install"]);
  });
});
