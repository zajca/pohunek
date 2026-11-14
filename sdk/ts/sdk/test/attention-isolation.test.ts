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
 * The `pohunek.attention.children=isolate` flag on `session.new` and
 * `session.set_metadata` is sent only to a daemon that advertises
 * `session_attention_isolation_supported`, because an older daemon stores
 * the key as plain metadata without isolating anything.
 */

const HOST = "netbird:attention-isolation";
const FLAG_METADATA = { "pohunek.attention.children": "isolate" };
const SETTLE_MS = 100;
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

function inspectStep(flags: Partial<HostCapabilities>): ScriptStep {
  return {
    kind: "gate",
    ready: Promise.resolve(),
    line: (line) => okResponseLine(requestIdFromLine(line), {
      daemon_version: "0.0.0-test",
      protocol_version: PROTOCOL_VERSION,
      supported_agents: [],
      runtimes: [],
      git_available: true,
      ...flags,
    }),
  };
}

function sessionStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), minimalSessionInfo()),
  };
}

function metadataStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), {
      session: minimalSessionInfo(),
      metadata: FLAG_METADATA,
    }),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("attention isolation tests require a real TCP endpoint");
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

describe("session.new attention isolation against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.new is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const params: SessionNewParams = { agent: "codex", cols: 80, rows: 24, metadata: FLAG_METADATA };
    const error = await rejection(client.call("session.new", params));

    expect(error.kind).toBe("sessionAttentionIsolationUnsupported");
    expect(error.code).toBe("session_attention_isolation_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the flagged creation", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_attention_isolation_supported: true }),
      sessionStep(),
    ]));
    const client = await connect(daemon);
    const params: SessionNewParams = { agent: "codex", cols: 80, rows: 24, metadata: FLAG_METADATA };
    await client.call("session.new", params);

    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.new"]);
    await client.close();
  });

  test("a session.new without the flag skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([sessionStep()]));
    const client = await connect(daemon);
    await client.call("session.new", { agent: "codex", cols: 80, rows: 24 });

    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["session.new"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});

describe("session.set_metadata attention isolation against a mock daemon", () => {
  test("setting the flag is refused without the capability flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.set_metadata", {
      session_id: "s-test-1",
      metadata: { "pohunek.attention.children": "isolate" },
    }));

    expect(error.kind).toBe("sessionAttentionIsolationUnsupported");
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("clearing the flag skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([metadataStep()]));
    const client = await connect(daemon);
    await client.call("session.set_metadata", {
      session_id: "s-test-1",
      metadata: { "pohunek.attention.children": null },
    });

    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["session.set_metadata"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });
});
