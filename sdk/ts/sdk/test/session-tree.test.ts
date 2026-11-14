import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionListParams } from "@pohunek/protocol";
import { Client, ClientError, connectTcp } from "@pohunek/sdk";
import {
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * Tree filters on `session.list` are sent only to a daemon that advertises
 * `session_list_tree_filters_supported`, because an older daemon ignores the
 * filters silently and returns the wrong set.
 */

const HOST = "netbird:session-tree";
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

function treeParams(): SessionListParams {
  return { filters: [{ key: "top_level" }] };
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

function listStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), [
      { ...minimalSessionInfo(), children: 2 },
    ]),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("session tree tests require a real TCP endpoint");
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

describe("session.list tree filters against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.list is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.list", treeParams()));

    expect(error.kind).toBe("sessionListTreeFiltersUnsupported");
    expect(error.code).toBe("session_list_tree_filters_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_list_tree_filters_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.list", treeParams()));
    expect(error.kind).toBe("sessionListTreeFiltersUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the filtered request", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_list_tree_filters_supported: true }),
      listStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.list", treeParams());

    expect(result.length).toBe(1);
    expect(result[0]?.children).toBe(2);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.list"]);
    expect((seen[1]?.["params"] as SessionListParams).filters).toEqual([{ key: "top_level" }]);
    await client.close();
  });

  test("a session.list without tree filters skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([listStep()]));
    const client = await connect(daemon);
    await client.call("session.list", { filters: [] });
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.list"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});
