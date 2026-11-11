import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionNewParams } from "@pohunek/protocol";
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
 * `context_package_digest` on `session.new` is sent only to a daemon that
 * advertises `session_new_context_pin_supported`, because an older daemon drops
 * the field silently and the session starts unpinned.
 */

const HOST = "netbird:context-pin";
const DIGEST = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
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
  return { agent: "codex", cols: 80, rows: 24, context_package_digest: DIGEST, ...extra };
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

function sessionStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), minimalSessionInfo()),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("context_package_digest tests require a real TCP endpoint");
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

describe("session.new context_package_digest against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.new is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));

    expect(error.kind).toBe("sessionNewContextPinUnsupported");
    expect(error.code).toBe("session_new_context_pin_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    expect(seen[0]?.["params"]).toEqual({ skip_runtime_probes: true });
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_new_context_pin_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewContextPinUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the digest", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_context_pin_supported: true }),
      sessionStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.new", newParams());

    expect(result.id).toBe(minimalSessionInfo().id);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.new"]);
    expect((seen[1]?.["params"] as SessionNewParams).context_package_digest).toBe(DIGEST);
    await client.close();
  });

  test("a session.new without a digest skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([sessionStep()]));
    const client = await connect(daemon);
    await client.call("session.new", { agent: "codex", cols: 80, rows: 24 });
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.new"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});
