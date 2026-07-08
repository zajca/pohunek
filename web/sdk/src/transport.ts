export interface ConnectOptions {
  connectTimeoutMs?: number;
  requestTimeoutMs?: number;
}

export interface ResolvedConnectOptions {
  connectTimeoutMs: number;
  requestTimeoutMs: number;
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

export function resolveConnectOptions(options: ConnectOptions = {}): ResolvedConnectOptions {
  return {
    connectTimeoutMs: options.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS,
  };
}
