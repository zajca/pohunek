import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionRemoveTreeParams } from "@pohunek/protocol";
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
 * `session.remove_tree` is sent only to a daemon that advertises
 * `session_remove_tree_supported`: an older daemon has no cascade, so the
 * client refuses before anything is removed.
 */

const HOST = "netbird:remove-tree";
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

function treeParams(): SessionRemoveTreeParams {
  return { session_id: "s-root" };
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

function treeStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), {
      removed: 1,
      kept: 0,
      refused: 0,
      sessions: [
        { session_id: "s-root", depth: 0, outcome: "removed", keep_reason: null },
      ],
    }),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("remove tree tests require a real TCP endpoint");
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

describe("session.remove_tree against a mock daemon", () => {
  test("a daemon without the capability flag is refused before the request is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.remove_tree", treeParams()));

    expect(error.kind).toBe("sessionRemoveTreeUnsupported");
    expect(error.code).toBe("session_remove_tree_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_remove_tree_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.remove_tree", treeParams()));

    expect(error.kind).toBe("sessionRemoveTreeUnsupported");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a capable daemon receives the tree request", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_remove_tree_supported: true }),
      treeStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.remove_tree", treeParams()) as {
      removed: number;
      sessions: Array<{ session_id: string; depth: number; outcome: string }>;
    };

    expect(result.removed).toBe(1);
    expect(result.sessions).toEqual([
      { session_id: "s-root", depth: 0, outcome: "removed", keep_reason: null },
    ]);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual([
      "host.inspect",
      "session.remove_tree",
    ]);
    await client.close();
  });

  test("the connect timeout covers the tree call", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_remove_tree_supported: true }),
      treeStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.remove_tree", treeParams()) as {
      removed: number;
    };
    expect(result.removed).toBe(1);
    await client.close();
  });
});
