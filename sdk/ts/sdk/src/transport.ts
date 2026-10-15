import { resolveRequestOrigin, type RequestOrigin } from "./origin";

export interface ConnectOptions {
  connectTimeoutMs?: number;
  requestTimeoutMs?: number;
  /** Explicit origin for a client hosted inside one managed session. */
  origin?: RequestOrigin;
}

export interface ResolvedConnectOptions {
  connectTimeoutMs: number;
  requestTimeoutMs: number;
  /** Present when the caller chose the response deadline explicitly. */
  explicitRequestTimeoutMs?: number;
  origin?: RequestOrigin;
}

export interface ControlChannel {
  send(line: string): Promise<void>;
  readonly lines: AsyncIterable<string>;
  close(): Promise<void>;
}

export interface RawDuplex {
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
  close(): Promise<void>;
}

export interface Transport {
  control(): Promise<ControlChannel>;
  raw(): Promise<RawDuplex>;
}

export const DEFAULT_CONNECT_TIMEOUT_MS = 5_000;
export const DEFAULT_REQUEST_TIMEOUT_MS = 5_000;
/** Budget for `session.new`: three 10-minute native service ceilings (worker
 * connection, initialization and, with initial input, reader grace), the
 * `pre-create` and `post-create` hooks with a host and a repository layer of
 * 5 minutes each, and 10 minutes for the rest of creation. */
export const SESSION_NEW_REQUEST_BUDGET_MS = 60 * 60 * 1_000;

export function resolveConnectOptions(options: ConnectOptions = {}): ResolvedConnectOptions {
  return {
    connectTimeoutMs: options.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS,
    ...(options.requestTimeoutMs === undefined ? {} : { explicitRequestTimeoutMs: options.requestTimeoutMs }),
    ...(options.origin === undefined ? {} : { origin: resolveRequestOrigin(options.origin) }),
  };
}
