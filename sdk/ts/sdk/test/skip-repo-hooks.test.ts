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
 * `skip_repo_hooks` on `session.new` is sent only to a daemon that advertises
 * `session_new_skip_repo_hooks_supported`, because an older daemon drops the
 * field silently and runs the repository's hooks.
 */

const HOST = "netbird:skip-repo-hooks";
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
  return { agent: "work", cols: 80, rows: 24, ...extra };
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
    throw new Error("skip_repo_hooks tests require a real TCP endpoint");
  }
  return connectTcp(
    HOST,
    { host: daemon.endpoint.host, port: daemon.endpoint.port },
    { requestTimeoutMs: REQUEST_TIMEOUT_MS },
  );
}

async function requestsSeen(daemon: MockDaemon, count: number): Promise<Array<Record<string, unknown>>> {
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

describe("session.new skip_repo_hooks against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.new is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams({ skip_repo_hooks: true })));

    expect(error.kind).toBe("sessionNewSkipRepoHooksUnsupported");
    expect(error.code).toBe("session_new_skip_repo_hooks_unsupported");
    expect(error.errorClass).toBe("daemon");
    const seen = await requestsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_skip_repo_hooks_supported: false }),
    ]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams({ skip_repo_hooks: true })));

    expect(error.kind).toBe("sessionNewSkipRepoHooksUnsupported");
    expect((await requestsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a non-boolean switch is gated like true", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const yes = { skip_repo_hooks: "yes" } as unknown as Partial<SessionNewParams>;
    const error = await rejection(client.call("session.new", newParams(yes)));

    expect(error.kind).toBe("sessionNewSkipRepoHooksUnsupported");
    expect((await requestsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives the switch", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_skip_repo_hooks_supported: true }),
      sessionStep(),
    ]));
    const client = await connect(daemon);
    const result = await client.call("session.new", newParams({ skip_repo_hooks: true }));

    expect(result.id).toBe(minimalSessionInfo().id);
    const seen = await requestsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.new"]);
    expect((seen[1]?.["params"] as SessionNewParams).skip_repo_hooks).toBe(true);
    await client.close();
  });

  test("a session.new with the switch false or absent skips the capability check", async () => {
    for (const params of [
      newParams(),
      newParams({ skip_repo_hooks: false }),
    ]) {
      const daemon = track(await startTcpDaemon([sessionStep()]));
      const client = await connect(daemon);
      await client.call("session.new", params);
      const seen = await requestsSeen(daemon, 1);
      expect(seen.map((request) => request["method"])).toEqual(["session.new"]);
      // The SDK forwards an explicit false unchanged; the daemon reads it as the default.
      const sent = seen[0]?.["params"] as Record<string, unknown>;
      expect(sent["skip_repo_hooks"] ?? false).toBe(false);
      await daemon.expectNoRequest(SETTLE_MS);
      await client.close();
    }
  });
});
