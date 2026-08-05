import { Buffer } from "node:buffer";
import { describe, expect, test } from "bun:test";
import { MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS, type ProtocolError, type ProtocolEvent, type SessionInfo } from "@pohunek/protocol";
import { startRelay, type DaemonTarget, type RelayHandle } from "@pohunek/backend";
import {
  Client,
  ClientError,
  attachRawWs,
  connectRawWs,
  type CatchAllEvent,
  type RawStream,
  type Request,
} from "@pohunek/sdk";
import {
  errResponseLine,
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startUnixDaemon,
  type MockDaemon,
} from "./mock-daemon";

const RELAY_HOST = "local";
const READY_BYTES = Uint8Array.of(0x70, 0x74, 0x79);
const BACKPRESSURE_TOTAL_BYTES = 4 * 1024 * 1024;
const BACKPRESSURE_CHUNK_BYTES = 64 * 1024;
const SLOW_READER_DELAY_MS = 2;
const SLOW_CLIENT_READ_DELAY_MS = 1;

interface RelayFixture {
  daemon: MockDaemon;
  relay: RelayHandle;
  client(): Promise<Client>;
  raw(): Promise<RawStream>;
  attach(streamId: string): Promise<RawStream>;
  close(): Promise<void>;
}

describe("WebSocket transport through relay", () => {
  test("call decodes a typed session.list output and sends method params", async () => {
    const session = minimalSessionInfo();
    const fixture = await startRelayFixture([
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), [session]) },
    ]);
    try {
      const client = await fixture.client();

      const result = await client.call("session.list", {
        filters: [{ key: "state", value: "running" }],
      });

      expect(result).toEqual([session] satisfies SessionInfo[]);
      const sent = parseRequestLine(await fixture.daemon.nextRequest());
      expect(sent["v"]).toEqual(SUPPORTED_PROTOCOL_VERSIONS);
      expect(sent["method"]).toBe("session.list");
      expect(sent["params"]).toEqual({ filters: [{ key: "state", value: "running" }] });
      expect(String(sent["id"])).toStartWith("sdk-session.list-");
    } finally {
      await fixture.close();
    }
  });

  test("err response rejects with a host-context protocol error", async () => {
    const source: ProtocolError = {
      class: "runtime",
      code: "agent_failed",
      msg: "agent failed during test",
      recover: "retry the request",
    };
    const fixture = await startRelayFixture([
      { kind: "reply", line: (requestLine) => errResponseLine(requestIdFromLine(requestLine), source) },
    ]);
    try {
      const client = await fixture.client();

      const error = await expectClientError(client.call("daemon.health", null));

      const structured = error.toProtocolError();
      expect(structured.class).toBe(source.class);
      expect(structured.code).toBe(source.code);
      expect(structured.recover).toBe(source.recover);
      expect(structured.msg).toContain(RELAY_HOST);
    } finally {
      await fixture.close();
    }
  });

  test("garbled JSON reply maps to remote daemon unavailable and poisons the connection", async () => {
    const fixture = await startRelayFixture([
      { kind: "garbled" },
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { status: "ok" }) },
    ]);
    try {
      const client = await fixture.client();

      const firstError = await expectClientError(client.call("daemon.health", null));
      expect(firstError.toProtocolError().class).toBe("daemon");
      expect(firstError.toProtocolError().code).toBe("remote_daemon_unavailable");
      await fixture.daemon.nextRequest();

      const secondError = await expectClientError(client.call("daemon.health", null));
      expect(secondError.toProtocolError().class).toBe("transport");
      expect(secondError.toProtocolError().code).toBe("framing");
      await fixture.daemon.expectNoRequest(50);
    } finally {
      await fixture.close();
    }
  });

  test("daemon close without a response maps like the remote socket transport", async () => {
    const fixture = await startRelayFixture([{ kind: "close" }]);
    try {
      const client = await fixture.client();

      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().class).toBe("daemon");
      expect(error.toProtocolError().code).toBe("remote_daemon_unavailable");
    } finally {
      await fixture.close();
    }
  });

  test("oversized daemon reply closes the WebSocket with a framing error", async () => {
    const fixture = await startRelayFixture([{ kind: "oversized" }]);
    try {
      const client = await fixture.client();

      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().class).toBe("daemon");
      expect(error.toProtocolError().code).toBe("remote_daemon_unavailable");
    } finally {
      await fixture.close();
    }
  });

  test("oversized request is rejected before the relay forwards it", async () => {
    const fixture = await startRelayFixture([{ kind: "silent" }]);
    try {
      const client = await fixture.client();
      const request: Request = {
        v: SUPPORTED_PROTOCOL_VERSIONS,
        id: "req-too-large-ws",
        method: "daemon.health",
        params: { payload: "x".repeat(MAX_CONTROL_LINE_BYTES + 1) },
      };

      const error = await expectClientError(client.request(request));

      expect(error.toProtocolError().class).toBe("transport");
      expect(error.toProtocolError().code).toBe("framing");
      await fixture.daemon.expectNoRequest(50);
    } finally {
      await fixture.close();
    }
  });

  test("request timeout rejects and poisons the connection", async () => {
    const fixture = await startRelayFixture([
      {
        kind: "delay",
        ms: 60,
        line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { status: "ok" }),
      },
    ]);
    try {
      const client = await fixture.client();

      const firstError = await expectClientError(client.call("daemon.health", null));
      expect(firstError.toProtocolError().class).toBe("daemon");
      expect(firstError.toProtocolError().code).toBe("remote_daemon_unavailable");
      await fixture.daemon.nextRequest();

      const secondError = await expectClientError(client.call("daemon.health", null));
      expect(secondError.toProtocolError().class).toBe("transport");
      expect(secondError.toProtocolError().code).toBe("framing");
      await fixture.daemon.expectNoRequest(50);
    } finally {
      await fixture.close();
    }
  });

  test("nextEvent yields typed events, catch-all unknown events, and null on close", async () => {
    const request = subscribeRequest("subscribe-events-ws");
    const events = [
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "session_created",
        session: minimalSessionInfo(),
      }),
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "agent_state",
        session_id: "s-test-1",
        activity: "blocked",
        source: "report",
      }),
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "future_event",
        id: "req-future",
        payload: "still visible",
      }),
    ];
    const fixture = await startRelayFixture([
      {
        kind: "subscription",
        ack: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { subscribed: true }),
        events,
      },
    ]);
    try {
      const client = await fixture.client();
      const subscription = await client.subscribe(request);

      const first = await subscription.nextEvent();
      expect((first as ProtocolEvent | null)?.event).toBe("session_created");

      const second = await subscription.nextEvent();
      expect((second as ProtocolEvent | null)?.event).toBe("agent_state");

      const third = await subscription.nextEvent();
      if (!isCatchAllEvent(third)) {
        throw new Error("expected unknown event to decode as catch-all");
      }
      expect(third.event).toBe("future_event");
      expect(catchAllPayload(third)).toBe("still visible");

      expect(await subscription.nextEvent()).toBeNull();
    } finally {
      await fixture.close();
    }
  });

  test("attachRawWs writes the exact prelude and round-trips binary payloads", async () => {
    const fixture = await startRelayFixture([
      { kind: "attachSuccess", emit: [READY_BYTES], echo: true },
    ]);
    try {
      const raw = await fixture.attach("stream-binary-ws");
      const reader = raw.readable.getReader();
      const writer = raw.writable.getWriter();
      const payload = patternedBytes(1024);

      expect(utf8(await fixture.daemon.nextAttachPrelude())).toBe('{"attach":"stream-binary-ws"}\n');
      expectBytes(await readExactly(reader, READY_BYTES.byteLength), READY_BYTES);
      await writer.write(payload);
      expectBytes(await readExactly(reader, payload.byteLength), payload);

      writer.releaseLock();
      reader.releaseLock();
      await raw.close();
    } finally {
      await fixture.close();
    }
  });

  test("connectRawWs opens an unframed attach stream", async () => {
    const fixture = await startRelayFixture([
      { kind: "attachSuccess", emit: [READY_BYTES], echo: true },
    ]);
    try {
      const raw = await fixture.raw();
      const writer = raw.writable.getWriter();
      await writer.write(utf8Bytes('{"attach":"stream-manual-ws"}\n'));
      writer.releaseLock();

      expect(utf8(await fixture.daemon.nextAttachPrelude())).toBe('{"attach":"stream-manual-ws"}\n');
      expectBytes(await readAll(raw, READY_BYTES.byteLength), READY_BYTES);

      await raw.close();
    } finally {
      await fixture.close();
    }
  });

  test("multi-megabyte attach round-trip respects WebSocket writer backpressure", async () => {
    const fixture = await startRelayFixture([
      {
        kind: "attachSuccess",
        emit: [READY_BYTES],
        echo: true,
        readDelayMs: SLOW_READER_DELAY_MS,
      },
    ]);
    try {
      const raw = await fixture.attach("stream-backpressure-ws");
      const reader = raw.readable.getReader();
      const writer = raw.writable.getWriter();
      const payload = patternedBytes(BACKPRESSURE_TOTAL_BYTES);

      expectBytes(await readExactly(reader, READY_BYTES.byteLength), READY_BYTES);
      const echoed = readExactlySlow(reader, payload.byteLength, SLOW_CLIENT_READ_DELAY_MS);
      const metrics = await writeWithBackpressure(writer, payload, BACKPRESSURE_CHUNK_BYTES);
      await writer.close();
      const received = await echoed;

      expectBytes(received, payload);
      expect(metrics.backpressureSignals).toBeGreaterThan(0);
      expect(metrics.maxOutstandingBytes <= BACKPRESSURE_CHUNK_BYTES).toBe(true);
      expect(metrics.minDesiredSize <= 0).toBe(true);

      reader.releaseLock();
      await raw.close();
    } finally {
      await fixture.close();
    }
  });

  test("failed attach redemption rejects with the daemon protocol error", async () => {
    const daemonError: ProtocolError = {
      class: "runtime",
      code: "attach_expired",
      msg: "attach stream expired during test",
      recover: "request a new attach stream",
    };
    const fixture = await startRelayFixture([
      { kind: "attachFailed", line: () => errResponseLine("attach-redemption", daemonError) },
    ]);
    try {
      const error = await expectClientError(fixture.attach("stream-expired-ws"));

      const structured = error.toProtocolError();
      expect(structured.class).toBe(daemonError.class);
      expect(structured.code).toBe(daemonError.code);
      expect(structured.recover).toBe(daemonError.recover);
      expect(structured.msg).toContain(RELAY_HOST);
      expect(utf8(await fixture.daemon.nextAttachPrelude())).toBe('{"attach":"stream-expired-ws"}\n');
    } finally {
      await fixture.close();
    }
  });

  test("closing the raw WebSocket tears down the daemon attach socket", async () => {
    const fixture = await startRelayFixture([
      { kind: "attachSuccess", emit: [READY_BYTES], echo: true },
    ]);
    try {
      const raw = await fixture.attach("stream-close-ws");
      await raw.close();

      await fixture.daemon.nextAttachPrelude();
      await fixture.daemon.expectNoRequest(50);
    } finally {
      await fixture.close();
    }
  });

  test("unreachable relay rejects connectWs with the host_unreachable taxonomy", async () => {
    const relay = await startRelay({
      bindHost: "127.0.0.1",
      port: 0,
      allowLoopbackBind: true,
      targets: new Map([[RELAY_HOST, { kind: "unix", socketPath: "/tmp/pohunek-sdk-missing.sock" }]]),
    });
    const url = relay.url;
    await relay.close();

    const error = await expectClientError(Client.connectWs(url, RELAY_HOST, { connectTimeoutMs: 100 }));

    const structured = error.toProtocolError();
    expect(structured.class).toBe("transport");
    expect(structured.code).toBe("host_unreachable");
  });
});

async function startRelayFixture(steps: Parameters<typeof startUnixDaemon>[0]): Promise<RelayFixture> {
  const daemon = await startUnixDaemon(steps);
  try {
    const target = daemonTarget(daemon);
    const relay = await startRelay({
      bindHost: "127.0.0.1",
      port: 0,
      allowLoopbackBind: true,
      targets: new Map([[RELAY_HOST, target]]),
    });
    return {
      daemon,
      relay,
      client: (): Promise<Client> => Client.connectWs(relay.url, RELAY_HOST, { requestTimeoutMs: 20 }),
      raw: (): Promise<RawStream> => connectRawWs(relay.url, RELAY_HOST),
      attach: (streamId: string): Promise<RawStream> => attachRawWs(relay.url, RELAY_HOST, streamId),
      close: async (): Promise<void> => {
        await relay.close();
        await daemon.close();
      },
    };
  } catch (error: unknown) {
    await daemon.close();
    throw error;
  }
}

function daemonTarget(daemon: MockDaemon): DaemonTarget {
  if (daemon.endpoint.kind === "unix") {
    return { kind: "unix", socketPath: daemon.endpoint.socketPath };
  }
  if (daemon.endpoint.kind === "tcp") {
    return { kind: "tcp", host: daemon.endpoint.host, port: daemon.endpoint.port };
  }
  throw new Error("WebSocket relay tests require a real socket daemon endpoint");
}

function subscribeRequest(id: string): Request {
  return {
    v: SUPPORTED_PROTOCOL_VERSIONS,
    id,
    method: "subscribe",
    params: null,
  };
}

function isCatchAllEvent(event: ProtocolEvent | CatchAllEvent | null): event is CatchAllEvent {
  return event !== null && event.event === "future_event";
}

function catchAllPayload(event: CatchAllEvent): unknown {
  return event["payload"];
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

async function readAll(raw: RawStream, byteLength: number): Promise<Uint8Array> {
  const reader = raw.readable.getReader();
  try {
    return await readExactly(reader, byteLength);
  } finally {
    reader.releaseLock();
  }
}

async function readExactly(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  byteLength: number,
): Promise<Uint8Array> {
  const output = new Uint8Array(byteLength);
  let offset = 0;
  while (offset < byteLength) {
    const next = await reader.read();
    if (next.done === true) {
      throw new Error(`raw stream closed after ${offset} of ${byteLength} bytes`);
    }
    if (offset + next.value.byteLength > byteLength) {
      throw new Error("raw stream returned more bytes than the test expected");
    }
    output.set(next.value, offset);
    offset += next.value.byteLength;
  }
  return output;
}

async function readExactlySlow(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  byteLength: number,
  delayMs: number,
): Promise<Uint8Array> {
  const output = new Uint8Array(byteLength);
  let offset = 0;
  while (offset < byteLength) {
    const next = await reader.read();
    if (next.done === true) {
      throw new Error(`raw stream closed after ${offset} of ${byteLength} bytes`);
    }
    if (offset + next.value.byteLength > byteLength) {
      throw new Error("raw stream returned more bytes than the test expected");
    }
    output.set(next.value, offset);
    offset += next.value.byteLength;
    await delay(delayMs);
  }
  return output;
}

interface BackpressureMetrics {
  backpressureSignals: number;
  maxOutstandingBytes: number;
  minDesiredSize: number;
}

async function writeWithBackpressure(
  writer: WritableStreamDefaultWriter<Uint8Array>,
  payload: Uint8Array,
  chunkSize: number,
): Promise<BackpressureMetrics> {
  let backpressureSignals = 0;
  let maxOutstandingBytes = 0;
  let minDesiredSize = Number.POSITIVE_INFINITY;

  for (let offset = 0; offset < payload.byteLength; offset += chunkSize) {
    const desiredBefore = writer.desiredSize;
    if (desiredBefore !== null) {
      minDesiredSize = Math.min(minDesiredSize, desiredBefore);
      if (desiredBefore <= 0) {
        backpressureSignals += 1;
        await writer.ready;
      }
    }

    const chunk = payload.subarray(offset, Math.min(offset + chunkSize, payload.byteLength));
    maxOutstandingBytes = Math.max(maxOutstandingBytes, chunk.byteLength);
    const write = writer.write(chunk);

    const desiredAfter = writer.desiredSize;
    if (desiredAfter !== null) {
      minDesiredSize = Math.min(minDesiredSize, desiredAfter);
      if (desiredAfter <= 0) {
        backpressureSignals += 1;
      }
    }

    await write;
  }

  return {
    backpressureSignals,
    maxOutstandingBytes,
    minDesiredSize,
  };
}

function expectBytes(actual: Uint8Array, expected: Uint8Array): void {
  expect(Buffer.compare(Buffer.from(actual), Buffer.from(expected))).toBe(0);
}

function patternedBytes(byteLength: number): Uint8Array {
  const bytes = new Uint8Array(byteLength);
  for (let index = 0; index < bytes.byteLength; index += 1) {
    bytes[index] = (index * 31 + 17) & 0xff;
  }
  bytes[0] = 0x00;
  bytes[1] = 0xff;
  bytes[2] = 0x80;
  return bytes;
}

function utf8(bytes: Uint8Array): string {
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
}

function utf8Bytes(value: string): Uint8Array {
  return new TextEncoder().encode(value);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
