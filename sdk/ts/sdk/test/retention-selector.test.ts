import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionRetentionParams } from "@pohunek/protocol";
import { Client, ClientError, connectTcp } from "@pohunek/sdk";
import {
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * Sweep selectors on `session.retention.sweep` are sent only to a daemon
 * that advertises `session_retention_selector_supported`, because an older
 * daemon ignores the selector silently and sweeps everything eligible.
 */

const HOST = "netbird:retention-selector";
const SETTLE_MS = 100;
// Equal to the suite hang guard: a lost reply fails the test instead of a short client deadline.
const REQUEST_TIMEOUT_MS = 120_000;

const open: Array<{ close(): Promise<void> }> = [];

afterEach(async () => {
  while (open.length > 0) {
    await open.pop()?.close();
  }
});

function track<T extends { close(): Promise<void> }>(resource: T): T {
  open.push(resource);
  return resource;
}

function selectorParams(): SessionRetentionParams {
  return { dry_run: true, spawned_by: "s-parent" };
}

/** Answers without closing the connection, so the next request can reuse it. */
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

function sweepStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), {
      dry_run: true,
      examined: 1,
      eligible: 0,
      held: 0,
      removed: 0,
      worktrees_cleaned: 0,
      worktrees_failed: 0,
      failed: 0,
      sessions: [],
    }),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("retention selector tests require a real TCP endpoint");
  }
  return connectTcp(
    HOST,
    { host: daemon.endpoint.host, port: daemon.endpoint.port },
    { requestTimeoutMs: REQUEST_TIMEOUT_MS },
  );
}

async function methodsSeen(daemon: MockDaemon, count: number): Promise<Array<Record<string, unknown>>> {
  const requests: Array<Record<string, unknown>> = [];
  for (let index = 0; index < count; index += 1) {
    requests.push(parseRequestLine(await daemon.nextRequest()));
  }
  return requests;
}

async function rejection(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(ClientError);
    return error as ClientError;
  }
  throw new Error("expected the call to reject with ClientError");
}

describe("session.retention.sweep selectors against a mock daemon", () => {
  test("a daemon without the capability flag is refused before the sweep is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.retention.sweep", selectorParams()));

    expect(error.kind).toBe("sessionRetentionSelectorUnsupported");
    expect(error.code).toBe("session_retention_selector_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_retention_selector_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.retention.sweep", selectorParams()));
    expect(error.kind).toBe("sessionRetentionSelectorUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the selective request", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_retention_selector_supported: true }),
      sweepStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.retention.sweep", selectorParams()) as { examined: number };

    expect(result.examined).toBe(1);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.retention.sweep"]);
    expect((seen[1]?.["params"] as Record<string, unknown>)["spawned_by"]).toBe("s-parent");
    await client.close();
  });

  test("a sweep without a selector skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([sweepStep()]));
    const client = await connect(daemon);
    await client.call("session.retention.sweep", { dry_run: true });
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.retention.sweep"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});
