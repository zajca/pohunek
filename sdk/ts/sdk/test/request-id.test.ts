import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionNewParams } from "@pohunek/protocol";
import { Client, ClientError, CompatError, connectTcp, downgradeRequestParams } from "@pohunek/sdk";
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
 * `client_request_id` on `session.new` is sent only to a daemon that advertises
 * `session_new_request_id_supported`, because an older daemon drops the key
 * silently and a retry would create a second session.
 */

const HOST = "netbird:request-id";
const KEY = "req-sdk-1";
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

function newParams(extra: Partial<SessionNewParams> = {}): SessionNewParams {
  return { agent: "codex", cols: 80, rows: 24, client_request_id: KEY, ...extra };
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

function replayedStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), { ...minimalSessionInfo(), replayed: true }),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("client_request_id tests require a real TCP endpoint");
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

describe("session.new client_request_id against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.new is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));

    expect(error.kind).toBe("sessionNewRequestIdUnsupported");
    expect(error.code).toBe("session_new_request_id_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    expect(seen[0]?.["params"]).toEqual({ skip_runtime_probes: true });
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_new_request_id_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewRequestIdUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the key and a replay decodes", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_request_id_supported: true }),
      replayedStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.new", newParams());

    expect(result.id).toBe(minimalSessionInfo().id);
    expect(result.replayed).toBe(true);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.new"]);
    expect((seen[1]?.["params"] as SessionNewParams).client_request_id).toBe(KEY);
    await client.close();
  });

  test("a session.new without a key skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([replayedStep()]));
    const client = await connect(daemon);
    await client.call("session.new", { agent: "codex", cols: 80, rows: 24 });
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.new"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});

describe("session.new client_request_id against the previous protocol", () => {
  test("the version adapter refuses the key instead of dropping it", () => {
    expect(() => downgradeRequestParams(3, "session.new", newParams())).toThrow(CompatError);
    const plain = { agent: "codex", cols: 80, rows: 24 };
    expect(downgradeRequestParams(3, "session.new", plain)).toEqual(plain);
    const nulled = { ...plain, client_request_id: null };
    expect(downgradeRequestParams(3, "session.new", nulled)).toEqual(nulled);
  });
});
