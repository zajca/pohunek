import type { AttachPrelude, ProtocolError } from "@pohunek/protocol";
import { ClientError } from "./error";
import { decodeResponse } from "./envelope";
import { SocketTransport } from "./transport-socket";
import type { ConnectOptions, RawDuplex, Transport } from "./transport";
import { WsTransport } from "./transport-ws";

export type RawStream = RawDuplex;

const LOCAL_HOST = "local";
const LINE_FEED = 0x0a;
const CARRIAGE_RETURN = 0x0d;

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

export async function connectRawLocal(socketPath: string, opts?: ConnectOptions): Promise<RawStream> {
  return connectRawTransport(SocketTransport.unix(socketPath, opts));
}

export async function connectRawTcp(
  host: string,
  addr: { host: string; port: number },
  opts?: ConnectOptions,
): Promise<RawStream> {
  return connectRawTransport(SocketTransport.tcp(host, addr, opts));
}

export async function connectRawWs(
  baseUrl: string,
  host: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  return connectRawTransport(WsTransport.relay(baseUrl, host, opts));
}

export async function connectRawTransport(transport: Transport): Promise<RawStream> {
  return transport.raw();
}

export async function attachRawTransport(
  transport: Transport,
  streamId: string,
  remoteHost?: string,
): Promise<RawStream> {
  const raw = await connectRawTransport(transport);
  return redeemAttach(raw, streamId, remoteHost);
}

export async function attachRaw(
  host: string,
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  if (isLocalHost(host)) {
    return attachRawLocal(socketPath, streamId, opts);
  }

  throw ClientError.hostUnreachable(
    host,
    "remote host resolution is not available in the TypeScript SDK core; use attachRawTcp with an explicit address",
  );
}

export async function attachRawLocal(
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  const raw = await connectRawLocal(socketPath, opts);
  return redeemAttach(raw, streamId);
}

export async function attachRawTcp(
  host: string,
  addr: { host: string; port: number },
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  const raw = await connectRawTcp(host, addr, opts);
  return redeemAttach(raw, streamId, host);
}

export async function attachRawWs(
  baseUrl: string,
  host: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  const raw = await connectRawWs(baseUrl, host, opts);
  return redeemAttach(raw, streamId, host);
}

async function redeemAttach(
  raw: RawDuplex,
  streamId: string,
  remoteHost?: string,
): Promise<RawStream> {
  await writeAttachPrelude(raw, streamId);
  const reader = raw.readable.getReader();

  let first: ReadableStreamReadResult<Uint8Array>;
  try {
    first = await reader.read();
  } catch (error: unknown) {
    reader.releaseLock();
    await closeQuietly(raw);
    throw mapUnknownError(error);
  }

  if (first.done === true) {
    reader.releaseLock();
    await closeQuietly(raw);
    throw noAttachResponseError(remoteHost);
  }

  const failed = decodeFailedRedemption(first.value, remoteHost);
  if (failed !== undefined) {
    reader.releaseLock();
    await closeQuietly(raw);
    throw failed;
  }

  return {
    readable: readableWithPrefix(reader, first.value),
    writable: raw.writable,
    close: (): Promise<void> => raw.close(),
  };
}

async function writeAttachPrelude(raw: RawDuplex, streamId: string): Promise<void> {
  const writer = raw.writable.getWriter();
  try {
    await writer.write(attachPreludeBytes(streamId));
  } catch (error: unknown) {
    await closeQuietly(raw);
    throw mapUnknownError(error);
  } finally {
    writer.releaseLock();
  }
}

function attachPreludeBytes(streamId: string): Uint8Array {
  const prelude: AttachPrelude = { attach: streamId };
  const line = JSON.stringify(prelude);
  if (line === undefined) {
    throw ClientError.json("attach prelude serialized to undefined");
  }
  return encoder.encode(`${line}\n`);
}

function decodeFailedRedemption(chunk: Uint8Array, remoteHost: string | undefined): ClientError | undefined {
  // Redemption failure is the only daemon path that sends a framed response on
  // the raw connection. A complete first error line rejects; every other first
  // chunk is replayed unchanged as opaque PTY bytes by readableWithPrefix.
  const newlineIndex = indexOfByte(chunk, LINE_FEED);
  if (newlineIndex < 0) {
    return undefined;
  }

  const lineBytes = trimCarriageReturn(chunk.subarray(0, newlineIndex));
  let line: string;
  try {
    line = decoder.decode(lineBytes);
  } catch {
    return undefined;
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(line) as unknown;
  } catch {
    return undefined;
  }

  try {
    const response = decodeResponse(parsed);
    if ("err" in response) {
      return mapDaemonError(remoteHost, response.err);
    }
  } catch {
    return undefined;
  }

  return undefined;
}

function readableWithPrefix(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  firstChunk: Uint8Array,
): ReadableStream<Uint8Array> {
  let prefix: Uint8Array | undefined = firstChunk;
  let released = false;

  const release = (): void => {
    if (!released) {
      released = true;
      reader.releaseLock();
    }
  };

  return new ReadableStream<Uint8Array>({
    async pull(controller): Promise<void> {
      if (prefix !== undefined) {
        controller.enqueue(prefix);
        prefix = undefined;
        return;
      }

      try {
        const next = await reader.read();
        if (next.done === true) {
          release();
          controller.close();
          return;
        }
        controller.enqueue(next.value);
      } catch (error: unknown) {
        release();
        controller.error(error);
      }
    },
    async cancel(reason: unknown): Promise<void> {
      prefix = undefined;
      try {
        await reader.cancel(reason);
      } finally {
        release();
      }
    },
  });
}

function mapDaemonError(remoteHost: string | undefined, error: ProtocolError): ClientError {
  if (remoteHost !== undefined) {
    return ClientError.remoteProtocol(remoteHost, error);
  }
  return ClientError.protocol(error);
}

function mapUnknownError(error: unknown): ClientError {
  if (error instanceof ClientError) {
    return error;
  }
  return ClientError.io(error);
}

function noAttachResponseError(remoteHost: string | undefined): ClientError {
  if (remoteHost !== undefined) {
    return ClientError.remoteDaemonUnavailable(remoteHost);
  }
  return ClientError.framing("daemon closed the attach connection before sending any raw bytes");
}

function isLocalHost(host: string): boolean {
  return host.length === 0 || host === LOCAL_HOST;
}

function indexOfByte(bytes: Uint8Array, needle: number): number {
  for (let index = 0; index < bytes.byteLength; index += 1) {
    if (bytes[index] === needle) {
      return index;
    }
  }
  return -1;
}

function trimCarriageReturn(bytes: Uint8Array): Uint8Array {
  if (bytes.at(-1) === CARRIAGE_RETURN) {
    return bytes.subarray(0, bytes.byteLength - 1);
  }
  return bytes;
}

async function closeQuietly(raw: RawDuplex): Promise<void> {
  try {
    await raw.close();
  } catch {
    // Best effort cleanup; the original attach error is more useful to callers.
  }
}
