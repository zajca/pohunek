import { attachRawTransport, connectRawTransport, type RawStream } from "./attach";
import { Client } from "./client";
import { ClientError } from "./error";
import type { ConnectOptions } from "./transport";
import { SocketTransport } from "./transport-socket";

const LOCAL_HOST = "local";

/** Connects a client to a daemon over its local Unix socket. */
export async function connectLocal(socketPath: string, opts?: ConnectOptions): Promise<Client> {
  return Client.connectTransport(SocketTransport.unix(socketPath, opts), opts);
}

/** Connects a client to a daemon at an explicit TCP address. */
export async function connectTcp(
  host: string,
  addr: { host: string; port: number },
  opts?: ConnectOptions,
): Promise<Client> {
  return Client.connectTransport(SocketTransport.tcp(host, addr, opts), opts, host);
}

/** Opens an unframed stream over a local Unix socket. */
export async function connectRawLocal(socketPath: string, opts?: ConnectOptions): Promise<RawStream> {
  return connectRawTransport(SocketTransport.unix(socketPath, opts));
}

/** Opens an unframed stream at an explicit TCP address. */
export async function connectRawTcp(
  host: string,
  addr: { host: string; port: number },
  opts?: ConnectOptions,
): Promise<RawStream> {
  return connectRawTransport(SocketTransport.tcp(host, addr, opts));
}

/** Redeems a local attach stream and rejects unresolved remote hosts. */
export async function attachRaw(
  host: string,
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  if (host.length === 0 || host === LOCAL_HOST) {
    return attachRawLocal(socketPath, streamId, opts);
  }

  throw ClientError.hostUnreachable(
    host,
    "remote host resolution is not available in the TypeScript SDK core; use attachRawTcp with an explicit address",
  );
}

/** Redeems an attach stream over a local Unix socket. */
export async function attachRawLocal(
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  return attachRawTransport(SocketTransport.unix(socketPath, opts), streamId);
}

/** Redeems an attach stream at an explicit TCP address. */
export async function attachRawTcp(
  host: string,
  addr: { host: string; port: number },
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  return attachRawTransport(SocketTransport.tcp(host, addr, opts), streamId, host);
}

export { SocketTransport };
