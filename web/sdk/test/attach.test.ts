import { Buffer } from "node:buffer";
import { describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type ProtocolError } from "@pohunek/protocol";
import {
  Client,
  ClientError,
  attachRawLocal,
  attachRawTcp,
  connectRawLocal,
  type RawStream,
} from "@pohunek/sdk";
import { attachRawTransport, connectRawTransport } from "../src/attach";
import {
  errResponseLine,
  okResponseLine,
  requestIdFromLine,
  startTcpDaemon,
  startUnixDaemon,
  type MockDaemon,
} from "./mock-daemon";

const READY_BYTES = Uint8Array.of(0x70, 0x74, 0x79);
const BACKPRESSURE_TOTAL_BYTES = 4 * 1024 * 1024;
const BACKPRESSURE_CHUNK_BYTES = 64 * 1024;
const SLOW_READER_DELAY_MS = 2;
const SLOW_CLIENT_READ_DELAY_MS = 1;

interface BackpressureMetrics {
  backpressureSignals: number;
  maxOutstandingBytes: number;
  minDesiredSize: number;
}

describe("attach raw stream", () => {
  test("attachRawLocal writes the exact attach prelude line", async () => {
    const streamId = "stream-exact";
    const daemon = await startUnixDaemon([
      { kind: "attachSuccess", emit: [READY_BYTES] },
    ]);
    try {
      const raw = await attachLocal(daemon, streamId);

      const prelude = await daemon.nextAttachPrelude();
      expect(utf8(prelude)).toBe('{"attach":"stream-exact"}\n');
      const parsed = JSON.parse(utf8(prelude.slice(0, -1))) as Record<string, unknown>;
      expect(Object.keys(parsed)).toEqual(["attach"]);
      expect(parsed["attach"]).toBe(streamId);
      expect(parsed["v"]).toBeUndefined();

      await raw.close();
    } finally {
      await daemon.close();
    }
  });

  test("attachRawTcp uses the raw TCP transport and writes the attach prelude", async () => {
    const streamId = "stream-tcp";
    const daemon = await startTcpDaemon([
      { kind: "attachSuccess", emit: [READY_BYTES] },
    ]);
    try {
      const raw = await attachTcp(daemon, "build-box", streamId);

      expect(utf8(await daemon.nextAttachPrelude())).toBe('{"attach":"stream-tcp"}\n');

      await raw.close();
    } finally {
      await daemon.close();
    }
  });

  test("connectRawLocal opens an unframed stream without writing a prelude", async () => {
    const streamId = "stream-manual";
    const daemon = await startUnixDaemon([
      { kind: "attachSuccess", emit: [READY_BYTES], echo: true },
    ]);
    try {
      const raw = await connectRaw(daemon);
      const writer = raw.writable.getWriter();
      await writer.write(utf8Bytes(`{"attach":"${streamId}"}\n`));
      writer.releaseLock();

      expect(utf8(await daemon.nextAttachPrelude())).toBe('{"attach":"stream-manual"}\n');
      expectBytes(await readAll(raw, READY_BYTES.byteLength), READY_BYTES);

      await raw.close();
    } finally {
      await daemon.close();
    }
  });

  test("binary payloads round-trip without UTF-8 assumptions", async () => {
    const daemon = await startUnixDaemon([
      { kind: "attachSuccess", emit: [READY_BYTES], echo: true },
    ]);
    try {
      const raw = await attachLocal(daemon, "stream-binary");
      const reader = raw.readable.getReader();
      const writer = raw.writable.getWriter();
      const payload = patternedBytes(1024);

      expectBytes(await readExactly(reader, READY_BYTES.byteLength), READY_BYTES);
      await writer.write(payload);
      expectBytes(await readExactly(reader, payload.byteLength), payload);

      writer.releaseLock();
      reader.releaseLock();
      await raw.close();
    } finally {
      await daemon.close();
    }
  });

  test("multi-megabyte round-trip respects writer backpressure", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "attachSuccess",
        emit: [READY_BYTES],
        echo: true,
        readDelayMs: SLOW_READER_DELAY_MS,
      },
    ]);
    try {
      const raw = await attachLocal(daemon, "stream-backpressure");
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
      await daemon.close();
    }
  });

  test("close ends only the raw stream and leaves an independent control connection usable", async () => {
    const daemon = await startUnixDaemon([
      { kind: "attachSuccess", emit: [READY_BYTES] },
      {
        kind: "reply",
        line: (requestLine) =>
          okResponseLine(requestIdFromLine(requestLine), {
            status: "ok",
            daemon_version: "0.0.0-test",
            protocol_version: PROTOCOL_VERSION,
          }),
      },
    ]);
    try {
      const raw = await attachLocal(daemon, "stream-close");
      await raw.close();

      const client = await connectControl(daemon);
      const health = await client.call("daemon.health", null);

      expect(health.protocol_version).toBe(PROTOCOL_VERSION);
    } finally {
      await daemon.close();
    }
  });

  test("failed attach redemption rejects with the daemon protocol error", async () => {
    const daemonError: ProtocolError = {
      class: "runtime",
      code: "attach_expired",
      msg: "attach stream expired during test",
      recover: "request a new attach stream",
    };
    const daemon = await startUnixDaemon([
      { kind: "attachFailed", line: () => errResponseLine("attach-redemption", daemonError) },
    ]);
    try {
      const error = await expectClientError(attachLocal(daemon, "stream-expired"));

      const structured = error.toProtocolError();
      expect(structured.class).toBe(daemonError.class);
      expect(structured.code).toBe(daemonError.code);
      expect(structured.recover).toBe(daemonError.recover);
      expect(utf8(await daemon.nextAttachPrelude())).toBe('{"attach":"stream-expired"}\n');
    } finally {
      await daemon.close();
    }
  });
});

async function expectClientError(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(ClientError);
    return error as ClientError;
  }
  throw new Error("expected promise to reject with ClientError");
}

function unixSocketPath(daemon: MockDaemon): string {
  if (daemon.endpoint.kind !== "unix") {
    throw new Error("attach tests require a real Unix socket endpoint");
  }
  return daemon.endpoint.socketPath;
}

function tcpAddress(daemon: MockDaemon): { host: string; port: number } {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("attach tests require a real TCP endpoint");
  }
  return { host: daemon.endpoint.host, port: daemon.endpoint.port };
}

async function attachLocal(daemon: MockDaemon, streamId: string): Promise<RawStream> {
  if (daemon.endpoint.kind === "memory") {
    return attachRawTransport(daemon.endpoint.transport, streamId);
  }
  return attachRawLocal(unixSocketPath(daemon), streamId);
}

async function attachTcp(daemon: MockDaemon, host: string, streamId: string): Promise<RawStream> {
  if (daemon.endpoint.kind === "memory") {
    return attachRawTransport(daemon.endpoint.transport, streamId, host);
  }
  return attachRawTcp(host, tcpAddress(daemon), streamId);
}

async function connectRaw(daemon: MockDaemon): Promise<RawStream> {
  if (daemon.endpoint.kind === "memory") {
    return connectRawTransport(daemon.endpoint.transport);
  }
  return connectRawLocal(unixSocketPath(daemon));
}

async function connectControl(daemon: MockDaemon): Promise<Client> {
  if (daemon.endpoint.kind === "unix") {
    return Client.connectLocal(daemon.endpoint.socketPath);
  }
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport);
  }
  return Client.connectTcp("build-box", { host: daemon.endpoint.host, port: daemon.endpoint.port });
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
