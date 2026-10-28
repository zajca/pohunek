import { afterEach, describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type HostCapabilities, type SessionNewParams } from "@pohunek/protocol";
import { startTestRelay, type TestRelayHandle } from "@pohunek/testkit/bun-relay";
import { createFixtureRootSync, startFixtureDaemon, type FixtureDaemonHandle } from "@pohunek/testkit";
import { rmSync } from "node:fs";
import { Client, ClientError, CompatError, type ControlChannel, type Transport, connectLocal, connectTcp, downgradeRequestParams } from "@pohunek/sdk";
import {
  errResponseLine,
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startMemoryDaemon,
  startTcpDaemon,
  startUnixDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * `secret_env` on `session.new` is sent only to a daemon that advertises
 * `session_new_secret_env_supported`, never through the relay, and never
 * appears in an error the client raises.
 */

const HOST = "netbird:secret-env";
const RELAY_HOST = "local";
const SENTINEL = "sentinel-secret-value-7f3a91";
const SLOT = "METER_TOKEN";
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
  return { agent: "codex", cols: 80, rows: 24, secret_env: { [SLOT]: SENTINEL }, ...extra };
}

/** Answers without closing the connection, so the next request can reuse it. */
function keepOpen(line: (requestLine: string) => string): ScriptStep {
  return { kind: "gate", ready: Promise.resolve(), line };
}

function healthStep(): ScriptStep {
  return {
    kind: "gate",
    ready: Promise.resolve(),
    line: (line) => okResponseLine(requestIdFromLine(line), {
      status: "ok",
      daemon_version: "0.0.0-test",
      protocol_version: PROTOCOL_VERSION,
    }),
  };
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

function sessionNewStep(): ScriptStep {
  return {
    kind: "reply",
    line: (line) => okResponseLine(requestIdFromLine(line), minimalSessionInfo()),
  };
}

async function connect(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("secret_env tests require a real TCP endpoint");
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

/** Every serialization an application could log or print for the error. */
function renderings(error: ClientError): string[] {
  return [
    error.message,
    String(error),
    error.stack ?? "",
    JSON.stringify(error.toProtocolError()),
    JSON.stringify(error),
    error.recoverHint() ?? "",
  ];
}

describe("session.new secret_env against a mock daemon", () => {
  test("a daemon without the capability flag is refused before session.new is sent", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));

    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect(error.code).toBe("session_new_secret_env_unsupported");
    expect(error.errorClass).toBe("daemon");
    for (const text of renderings(error)) {
      expect(text.includes(SENTINEL)).toBe(false);
      expect(text.includes(SLOT)).toBe(false);
    }
    const seen = await methodsSeen(daemon, 1);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect"]);
    expect(seen[0]?.["params"]).toEqual({ skip_runtime_probes: true });
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a flag reported false is refused like an absent flag", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_new_secret_env_supported: false })]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a daemon with the flag receives secret_env and returns the session", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_secret_env_supported: true }),
      sessionNewStep(),
    ]));
    const client = await connect(daemon);
    const session = await client.call("session.new", newParams());

    expect(session.id).toBe(minimalSessionInfo().id);
    const seen = await methodsSeen(daemon, 2);
    expect(seen.map((request) => request["method"])).toEqual(["host.inspect", "session.new"]);
    expect((seen[1]?.["params"] as SessionNewParams).secret_env).toEqual({ [SLOT]: SENTINEL });
    await client.close();
  });

  test("a session.new sent through Client.request is gated the same way", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.request({
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: "raw-1",
      method: "session.new",
      params: newParams(),
    }));
    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"]))
      .toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a session.new without secret_env skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([sessionNewStep()]));
    const client = await connect(daemon);
    await client.call("session.new", { agent: "codex", cols: 80, rows: 24 });
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.new"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("an empty secret_env skips the capability check", async () => {
    const daemon = track(await startTcpDaemon([sessionNewStep()]));
    const client = await connect(daemon);
    await client.call("session.new", newParams({ secret_env: {} }));
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["session.new"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("Client.subscribe is gated before session.new is written", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({})]));
    const client = await connect(daemon);
    const error = await rejection(client.subscribe({
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: "sub-1",
      method: "session.new",
      params: newParams(),
    }).then(() => undefined));
    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("a non-object secret_env still triggers the capability check", async () => {
    for (const secretEnv of [null, ["a"], "a"]) {
      const daemon = track(await startTcpDaemon([inspectStep({})]));
      const client = await connect(daemon);
      const error = await rejection(client.call(
        "session.new",
        newParams({ secret_env: secretEnv as unknown as { [key in string]: string } }),
      ));
      expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
      expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
      await client.close();
    }
  });

  test("an error the daemon returns for secret_env never carries the sentinel on the client side", async () => {
    const daemon = track(await startTcpDaemon([
      inspectStep({ session_new_secret_env_supported: true }),
      {
        kind: "reply",
        line: (line) => errResponseLine(requestIdFromLine(line), {
          class: "runtime",
          code: "secret_env_not_declared",
          msg: "the launch profile does not declare the requested secret_env variable",
        }),
      },
    ]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));

    expect(error.code).toBe("secret_env_not_declared");
    for (const text of renderings(error)) {
      expect(text.includes(SENTINEL)).toBe(false);
    }
    await client.close();
  });

  test("a payload that fails to serialize does not echo the secret", async () => {
    const daemon = track(await startTcpDaemon([inspectStep({ session_new_secret_env_supported: true })]));
    const client = await connect(daemon);
    const circular: Record<string, unknown> = { agent: "codex", cols: 80, rows: 24, secret_env: { [SLOT]: SENTINEL } };
    circular["self"] = circular;
    const error = await rejection(client.call("session.new", circular as unknown as SessionNewParams));
    for (const text of renderings(error)) {
      expect(text.includes(SENTINEL)).toBe(false);
    }
    await client.close();
  });
});

describe("session.new secret_env over the relay transport", () => {
  test("is refused before any frame is sent to the host", async () => {
    const daemon = track(await startUnixDaemon([healthStep()]));
    if (daemon.endpoint.kind !== "unix") {
      throw new Error("the relay test requires a real Unix socket daemon endpoint");
    }
    const relay: TestRelayHandle = track(await startTestRelay({
      bindHost: "127.0.0.1",
      port: 0,
      targets: new Map([[RELAY_HOST, { kind: "unix", socketPath: daemon.endpoint.socketPath }]]),
    }));
    const client = track(await Client.connectWs(relay.url, RELAY_HOST, { requestTimeoutMs: REQUEST_TIMEOUT_MS }));

    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewSecretEnvRelayRefused");
    expect(error.code).toBe("session_new_secret_env_relay_refused");
    for (const text of renderings(error)) {
      expect(text.includes(SENTINEL)).toBe(false);
      expect(text.includes(SLOT)).toBe(false);
    }
    const rawError = await rejection(client.request({
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: "raw-relay-1",
      method: "session.new",
      params: newParams(),
    }));
    expect(rawError.kind).toBe("sessionNewSecretEnvRelayRefused");
    const subscribeError = await rejection(client.subscribe({
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: "sub-relay-1",
      method: "session.new",
      params: newParams(),
    }).then(() => undefined));
    expect(subscribeError.kind).toBe("sessionNewSecretEnvRelayRefused");
    await daemon.expectNoRequest(SETTLE_MS);
  });
});

describe("session.new secret_env over a relayed transport with a poisoned connection", () => {
  test("is refused without opening a replacement connection", async () => {
    const inner = startMemoryDaemon([{ kind: "silent" }]);
    if (inner.endpoint.kind !== "memory") {
      throw new Error("the memory daemon must expose a memory endpoint");
    }
    const wrapped = inner.endpoint.transport;
    let controls = 0;
    const relayed: Transport = {
      relayed: true,
      control: (): Promise<ControlChannel> => {
        controls += 1;
        return wrapped.control();
      },
      raw: () => wrapped.raw(),
    };
    // timing-allowed: #1691 a short request deadline poisons the shared connection
    const client = track(await Client.connectTransport(relayed, { requestTimeoutMs: 20 }, RELAY_HOST));
    const timedOut = await rejection(client.call("daemon.health", null));
    expect(timedOut.kind).toBe("requestTimeout");
    expect(controls).toBe(1);

    const request = {
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: "poisoned-1",
      method: "session.new",
      params: newParams(),
    };
    expect((await rejection(client.request(request))).kind).toBe("sessionNewSecretEnvRelayRefused");
    expect((await rejection(client.subscribe(request).then(() => undefined))).kind)
      .toBe("sessionNewSecretEnvRelayRefused");
    expect(controls).toBe(1);
  });
});

describe("session.new secret_env against the previous protocol", () => {
  test("a protocol 3 daemon gets the distinct refusal and no session.new", async () => {
    const daemon = track(await startTcpDaemon([{
      kind: "gate",
      ready: Promise.resolve(),
      line: (line) => JSON.stringify({
        v: 3,
        id: requestIdFromLine(line),
        ok: { daemon_version: "0.37.0", protocol_version: 3, supported_agents: [], runtimes: [], git_available: true },
      }),
    }]));
    const client = await connect(daemon);
    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect(error.code).toBe("session_new_secret_env_unsupported");
    expect((await methodsSeen(daemon, 1)).map((request) => request["method"])).toEqual(["host.inspect"]);
    await daemon.expectNoRequest(SETTLE_MS);
    await client.close();
  });

  test("the version adapter refuses the field instead of dropping it", () => {
    expect(() => downgradeRequestParams(3, "session.new", newParams())).toThrow(CompatError);
    const plain = { agent: "codex", cols: 80, rows: 24 };
    expect(downgradeRequestParams(3, "session.new", plain)).toEqual(plain);
    const emptied = { ...plain, secret_env: {} };
    expect(downgradeRequestParams(3, "session.new", emptied)).toEqual(emptied);
  });
});

describe("session.new secret_env against the fixture daemon", () => {
  async function startFixture(
    capabilities?: (base: HostCapabilities) => HostCapabilities,
  ): Promise<{ daemon: FixtureDaemonHandle; socketPath: string }> {
    const probe = await startFixtureDaemon({ listen: { unixSocketPath: fixtureSocket("probe") } });
    const base = await (async (): Promise<HostCapabilities> => {
      const client = await connectLocal(requireSocket(probe));
      try {
        return await client.call("host.inspect", null);
      } finally {
        await client.close();
      }
    })();
    await probe.close();
    const daemon = track(await startFixtureDaemon({
      listen: { unixSocketPath: fixtureSocket("daemon") },
      ...(capabilities === undefined ? {} : { host: { capabilities: capabilities(base) } }),
    }));
    return { daemon, socketPath: requireSocket(daemon) };
  }

  test("advertises the flag, creates the session and never shows the value", async () => {
    const { socketPath } = await startFixture();
    const client = track(await connectLocal(socketPath));
    const capabilities = await client.call("host.inspect", { skip_runtime_probes: true });
    expect(capabilities.session_new_secret_env_supported).toBe(true);

    const created = await client.call("session.new", newParams());
    const listed = await client.call("session.list", {});
    const inspected = await client.call("session.inspect", created.id);
    const everything = JSON.stringify({ created, listed, inspected });
    expect(everything.includes(SENTINEL)).toBe(false);
    expect(everything.includes(SLOT)).toBe(false);
  });

  test("rejects a secret_env that is not a string map", async () => {
    const { socketPath } = await startFixture();
    const client = track(await connectLocal(socketPath));
    const error = await rejection(client.call(
      "session.new",
      newParams({ secret_env: { [SLOT]: 1 } as unknown as { [key in string]: string } }),
    ));
    expect(error.code).toBe("bad_request");
    expect(renderings(error).some((text) => text.includes(SENTINEL))).toBe(false);
    expect(await client.call("session.list", {})).toEqual([]);
  });

  test("a fixture without the flag makes the client refuse and create no session", async () => {
    const { socketPath } = await startFixture((base) => ({ ...base, session_new_secret_env_supported: false }));
    const client = track(await connectLocal(socketPath));
    const error = await rejection(client.call("session.new", newParams()));
    expect(error.kind).toBe("sessionNewSecretEnvUnsupported");
    expect(await client.call("session.list", {})).toEqual([]);
  });
});

let fixtureCounter = 0;

function fixtureSocket(label: string): string {
  fixtureCounter += 1;
  return `${fixtureRoot()}/${label}-${fixtureCounter}.sock`;
}

let root: string | undefined;

function fixtureRoot(): string {
  if (root === undefined) {
    root = createFixtureRootSync("pk-secret-env-");
    const created = root;
    process.on("exit", (): void => {
      rmSync(created, { recursive: true, force: true });
    });
  }
  return root;
}

function requireSocket(daemon: FixtureDaemonHandle): string {
  if (daemon.unixSocketPath === undefined) {
    throw new Error("the fixture daemon has no Unix socket");
  }
  return daemon.unixSocketPath;
}
