import { afterEach, describe, expect, jest, test } from "bun:test";
import { MIN_PROTOCOL_VERSION, PROTOCOL_VERSION } from "@pohunek/protocol";
import {
  CLIENT_PROTOCOL_VERSIONS,
  Client,
  ClientError,
  nextRequestId,
  type ControlChannel,
  type Transport,
} from "@pohunek/sdk";
import { errResponseLine, okResponseLine, parseRequestLine, requestIdFromLine } from "./mock-daemon";

/**
 * A client whose request timed out opens a fresh control connection on its next
 * call. Every scenario drives the production client against a transport whose
 * channels the test controls line by line, on a virtual clock: the test decides
 * which channel a reply arrives on and when, so no scenario waits for real time.
 */

const REQUEST_TIMEOUT_MS = 1_000;
const CONNECT_TIMEOUT_MS = 2_000;
const HEALTH = { status: "ok", daemon_version: "test", protocol_version: PROTOCOL_VERSION };
const UNINSTALL = { digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000" };
/** Microtask turns that let the client run to its next await on a channel. */
const SETTLE_TURNS = 64;

class FakeChannel implements ControlChannel {
  public readonly sent: string[] = [];
  public closed = false;
  /** Makes `close()` never resolve, as a wedged WebSocket does. */
  public hangClose = false;
  public readonly lines: AsyncIterable<string>;
  private readonly inbox: string[] = [];
  private readonly waiting: Array<(result: IteratorResult<string>) => void> = [];
  private ended = false;

  public constructor() {
    this.lines = { [Symbol.asyncIterator]: () => ({ next: () => this.next() }) };
  }

  public push(line: string): void {
    const resolve = this.waiting.shift();
    if (resolve !== undefined) {
      resolve({ done: false, value: line });
      return;
    }
    this.inbox.push(line);
  }

  public send(line: string): Promise<void> {
    if (this.closed) {
      return Promise.reject(new Error("channel is closed"));
    }
    this.sent.push(line);
    return Promise.resolve();
  }

  public async close(): Promise<void> {
    this.closed = true;
    this.ended = true;
    for (const resolve of this.waiting.splice(0)) {
      resolve({ done: true, value: undefined });
    }
    if (this.hangClose) {
      await new Promise<void>(() => {});
    }
  }

  private next(): Promise<IteratorResult<string>> {
    const line = this.inbox.shift();
    if (line !== undefined) {
      return Promise.resolve({ done: false, value: line });
    }
    if (this.ended) {
      return Promise.resolve({ done: true, value: undefined });
    }
    return new Promise((resolve) => {
      this.waiting.push(resolve);
    });
  }

  /** Answers the request at `index` with `result` at the protocol `version`. */
  public answer(index: number, result: unknown, version: number = PROTOCOL_VERSION): void {
    const id = requestIdFromLine(this.requestLine(index));
    this.push(JSON.stringify({ v: version, id, ok: result }));
  }

  public methods(): string[] {
    return this.sent.map((line) => String(parseRequestLine(line)["method"]));
  }

  public requestLine(index: number): string {
    const line = this.sent[index];
    if (line === undefined) {
      throw new Error(`the channel never received request ${index}: ${this.methods().join(",")}`);
    }
    return line;
  }
}

class FakeTransport implements Transport {
  public readonly channels: FakeChannel[] = [];
  /** Error the next `control()` rejects with, once. */
  public failNext: Error | undefined;
  /** Holds the next `control()` until the promise resolves, once. */
  public gateNext: Promise<void> | undefined;
  /** Observed order of channel opens and closes. */
  public readonly events: string[] = [];

  public async control(): Promise<ControlChannel> {
    const gate = this.gateNext;
    this.gateNext = undefined;
    if (gate !== undefined) {
      await gate;
    }
    if (this.failNext !== undefined) {
      const error = this.failNext;
      this.failNext = undefined;
      throw error;
    }
    const channel = new FakeChannel();
    const index = this.channels.length;
    this.channels.push(channel);
    this.events.push(`open ${index}`);
    const close = channel.close.bind(channel);
    channel.close = async (): Promise<void> => {
      this.events.push(`close ${index}`);
      await close();
    };
    return channel;
  }

  public raw(): never {
    throw new Error("the reconnect scenarios use control channels only");
  }

  public channel(index: number): FakeChannel {
    const channel = this.channels[index];
    if (channel === undefined) {
      throw new Error(`channel ${index} was never opened (${this.channels.length} opened)`);
    }
    return channel;
  }
}

async function settle(): Promise<void> {
  for (let turn = 0; turn < SETTLE_TURNS; turn += 1) {
    await Promise.resolve();
  }
}

/** Lets the clock pass `ms` and the promises it releases run. */
async function elapse(ms: number): Promise<void> {
  jest.advanceTimersByTime(ms);
  await settle();
}

async function expectClientError(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(ClientError);
    return error as ClientError;
  }
  throw new Error("expected promise to reject with ClientError");
}

async function connect(transport: FakeTransport): Promise<Client> {
  return Client.connectTransport(transport, {
    requestTimeoutMs: REQUEST_TIMEOUT_MS,
    connectTimeoutMs: CONNECT_TIMEOUT_MS,
  });
}

/** Calls `daemon.health` and lets it time out on `channel`'s generation. */
async function timeOutOneCall(client: Client, channel: FakeChannel): Promise<ClientError> {
  const call = client.call("daemon.health", null);
  await settle();
  expect(channel.sent.length).toBeGreaterThan(0);
  await elapse(REQUEST_TIMEOUT_MS);
  return expectClientError(call);
}

/** A request whose parameters depend on the protocol version, so it probes first. */
function uninstallRequest(): Parameters<Client["request"]>[0] {
  return {
    v: CLIENT_PROTOCOL_VERSIONS,
    id: nextRequestId("package.uninstall"),
    method: "package.uninstall",
    params: UNINSTALL,
  };
}

afterEach(() => {
  jest.useRealTimers();
});

describe("a client recovers from a timed-out request", () => {
  test("B1 the call after a timeout succeeds with its own reply", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);

    const first = await timeOutOneCall(client, transport.channel(0));
    expect(first.kind).toBe("requestTimeout");

    const second = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await second).toEqual(HEALTH);
    await client.close();
  });

  test("B2 a late reply to the timed-out request never reaches the next call", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    const second = client.call("daemon.health", null);
    await settle();
    // The daemon answers the first request on the old connection only now.
    const stale = { ...HEALTH, daemon_version: "stale" };
    transport.channel(0).push(
      okResponseLine(requestIdFromLine(transport.channel(0).requestLine(0)), stale),
    );
    await settle();
    transport.channel(1).answer(0, HEALTH);

    expect(await second).toEqual(HEALTH);
    await client.close();
  });

  test("B3 a late timeout of the old connection cannot poison the new one", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const slow = client.call("daemon.health", null);
    await settle();
    await elapse(REQUEST_TIMEOUT_MS / 2);
    const fast = client.call("daemon.health", null);
    await settle();
    // The first call times out; the second, started later on the same
    // connection, is still pending.
    await elapse(REQUEST_TIMEOUT_MS / 2);
    await expectClientError(slow);

    const third = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await third).toEqual(HEALTH);

    // The second call's own timer fires now, on the old connection.
    await elapse(REQUEST_TIMEOUT_MS);
    await expectClientError(fast);
    const fourth = client.call("daemon.health", null);
    await settle();
    transport.channel(1).answer(1, HEALTH);
    expect(await fourth).toEqual(HEALTH);
    expect(transport.channels.length).toBe(2);
    await client.close();
  });

  test("B4 concurrent callers share one reopen", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    const calls = [client.call("daemon.health", null), client.call("daemon.health", null)];
    await settle();
    expect(transport.channels.length).toBe(2);
    expect(transport.channel(1).sent.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    transport.channel(1).answer(1, HEALTH);
    expect(await Promise.all(calls)).toEqual([HEALTH, HEALTH]);
    await client.close();
  });

  test("B5 the reopened connection learns the daemon version again", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    // Connection 0 selects the current version and then the request times out.
    const first = client.request(uninstallRequest());
    await settle();
    expect(transport.channel(0).methods()).toEqual(["daemon.health"]);
    transport.channel(0).answer(0, HEALTH);
    await settle();
    expect(transport.channel(0).methods()).toEqual(["daemon.health", "package.uninstall"]);
    await elapse(REQUEST_TIMEOUT_MS);
    expect((await expectClientError(first)).kind).toBe("requestTimeout");

    // The daemon restarted on the previous release, which has no such method.
    const second = client.request(uninstallRequest());
    await settle();
    expect(transport.channel(1).methods()).toEqual(["daemon.health"]);
    transport.channel(1).answer(0, { ...HEALTH, protocol_version: MIN_PROTOCOL_VERSION }, MIN_PROTOCOL_VERSION);

    expect((await expectClientError(second)).kind).toBe("daemonProtocolTooOld");
    expect(transport.channel(1).methods()).toEqual(["daemon.health"]);
    await client.close();
  });

  test("B6 the version probe has its own budget", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const call = client.request(uninstallRequest());
    await settle();
    // The probe and the request each take 80% of the request timeout.
    await elapse(REQUEST_TIMEOUT_MS * 0.8);
    transport.channel(0).answer(0, HEALTH);
    await settle();
    await elapse(REQUEST_TIMEOUT_MS * 0.8);
    transport.channel(0).answer(1, { removed: true });

    expect(await call).toEqual({ removed: true });
    await client.close();
  });

  test("B7 an unanswered probe times out the call and the next call reopens", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const call = client.request(uninstallRequest());
    await settle();
    await elapse(REQUEST_TIMEOUT_MS);
    const error = await expectClientError(call);
    expect(error.kind).toBe("requestTimeout");
    expect(error.message).toContain("version probe");
    expect(transport.channel(0).methods()).toEqual(["daemon.health"]);

    const next = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await next).toEqual(HEALTH);
    await client.close();
  });

  test("B8 a failed reopen is typed and the next call reopens again", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    transport.failNext = ClientError.daemonUnreachable("/run/pohunek.sock", new Error("gone"));
    const second = await expectClientError(client.call("daemon.health", null));
    expect(second.kind).toBe("daemonUnreachable");

    const third = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await third).toEqual(HEALTH);
    await client.close();
  });

  test("B9 a closed client never reopens", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));
    await client.close();

    const error = await expectClientError(client.call("daemon.health", null));
    expect(error.kind).toBe("framing");
    expect(error.message).toContain("closed");
    expect(transport.channels.length).toBe(1);
  });

  test("B10 a client whose subscription took the channel never reopens", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const subscribing = client.subscribe({
      v: CLIENT_PROTOCOL_VERSIONS,
      id: nextRequestId("subscribe"),
      method: "subscribe",
      params: null,
    });
    await settle();
    transport.channel(0).answer(0, { subscribed: true });
    await subscribing;

    const error = await expectClientError(client.call("daemon.health", null));
    expect(error.message).toContain("subscription consumed");
    expect(transport.channels.length).toBe(1);
  });

  test("B11 a dedicated call after a timeout runs on its own connection", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    const waited = client.call("session.wait", { session_id: "s-1", timeout_ms: 500 });
    await settle();
    // The wait opened its own connection (channel 1) and left the shared one alone.
    expect(transport.channels.length).toBe(2);
    expect(transport.channel(1).methods()).toEqual(["session.wait"]);
    transport.channel(1).answer(0, { session: null });
    await waited;
    expect(transport.channel(0).methods()).toEqual(["daemon.health"]);
    await client.close();
  });

  test("B12 an incompatible protocol version stays refused without a reopen", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const first = client.call("daemon.health", null);
    await settle();
    transport.channel(0).answer(0, HEALTH, PROTOCOL_VERSION + 1);
    expect((await expectClientError(first)).kind).toBe("versionMismatch");

    const second = await expectClientError(client.call("daemon.health", null));
    expect(second.kind).toBe("framing");
    expect(transport.channels.length).toBe(1);
    await client.close();
  });

  test("B13 a reply with another id reopens on the next call", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const first = client.call("daemon.health", null);
    await settle();
    transport.channel(0).push(okResponseLine("sdk-unrelated-0", HEALTH));
    expect((await expectClientError(first)).kind).toBe("framing");

    const second = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await second).toEqual(HEALTH);
    await client.close();
  });

  test("B14 an unparsable reply reopens on the next call", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const first = client.call("daemon.health", null);
    await settle();
    transport.channel(0).push("not json");
    expect((await expectClientError(first)).kind).toBe("json");

    const second = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await second).toEqual(HEALTH);
    await client.close();
  });

  test("B15 the old channel is closed before the new one opens", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    const second = client.call("daemon.health", null);
    await settle();
    transport.channel(1).answer(0, HEALTH);
    await second;
    expect(transport.events).toEqual(["open 0", "close 0", "open 1"]);
    await client.close();
  });

  test("B16 a timed-out read is not sent again", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const list = client.call("session.list", {});
    await settle();
    await elapse(REQUEST_TIMEOUT_MS);

    expect((await expectClientError(list)).kind).toBe("requestTimeout");
    await elapse(REQUEST_TIMEOUT_MS * 4);
    expect(transport.channel(0).methods()).toEqual(["session.list"]);
    expect(transport.channels.length).toBe(1);
    await client.close();
  });

  test("B20 a failed subscription leaves the client reopenable", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const subscribing = client.subscribe({
      v: CLIENT_PROTOCOL_VERSIONS,
      id: nextRequestId("subscribe"),
      method: "subscribe",
      params: null,
    });
    await settle();
    await elapse(REQUEST_TIMEOUT_MS);
    expect((await expectClientError(subscribing)).kind).toBe("requestTimeout");

    const call = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await call).toEqual(HEALTH);
    await client.close();
  });

  test("B21 a close that never finishes does not hold the reopen", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    transport.channel(0).hangClose = true;
    await timeOutOneCall(client, transport.channel(0));

    const second = client.call("daemon.health", null);
    await settle();
    expect(transport.channels.length).toBe(1);
    await elapse(CONNECT_TIMEOUT_MS);
    expect(transport.channels.length).toBe(2);
    transport.channel(1).answer(0, HEALTH);
    expect(await second).toEqual(HEALTH);
    await client.close();
  });

  test("B22 a refused subscription on an incompatible version stays refused", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const subscribing = client.subscribe({
      v: CLIENT_PROTOCOL_VERSIONS,
      id: nextRequestId("subscribe"),
      method: "subscribe",
      params: null,
    });
    await settle();
    transport.channel(0).answer(0, { subscribed: true }, PROTOCOL_VERSION + 1);
    expect((await expectClientError(subscribing)).kind).toBe("versionMismatch");

    const next = await expectClientError(client.call("daemon.health", null));
    expect(next.kind).toBe("framing");
    expect(transport.channels.length).toBe(1);
    await client.close();
  });

  test("B23 a client closed while it reconnects does not keep the new connection", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    await timeOutOneCall(client, transport.channel(0));

    let release: () => void = () => undefined;
    transport.gateNext = new Promise<void>((resolve) => {
      release = resolve;
    });
    const call = client.call("daemon.health", null);
    await settle();
    await client.close();
    release();

    expect((await expectClientError(call)).kind).toBe("framing");
    expect(transport.channel(1).closed).toBe(true);
  });

  test("a daemon error reply does not poison the connection", async () => {
    jest.useFakeTimers();
    const transport = new FakeTransport();
    const client = await connect(transport);
    const first = client.call("daemon.health", null);
    await settle();
    const id = requestIdFromLine(transport.channel(0).requestLine(0));
    transport.channel(0).push(errResponseLine(id, { class: "daemon", code: "bad_request", msg: "no" }));
    await expectClientError(first);

    const second = client.call("daemon.health", null);
    await settle();
    transport.channel(0).answer(1, HEALTH);
    expect(await second).toEqual(HEALTH);
    expect(transport.channels.length).toBe(1);
    await client.close();
  });
});
