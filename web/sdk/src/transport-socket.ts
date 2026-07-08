import { createConnection, type Socket } from "node:net";
import { Readable, Writable } from "node:stream";
import { ClientError } from "./error";
import { encodeControlLine, readControlLines } from "./framing";
import type { ConnectOptions, ControlChannel, RawDuplex, ResolvedConnectOptions, Transport } from "./transport";
import { resolveConnectOptions } from "./transport";

type SocketTarget =
  | { kind: "unix"; socketPath: string }
  | { kind: "tcp"; hostContext: string; host: string; port: number };

export class SocketTransport implements Transport {
  private readonly target: SocketTarget;
  private readonly options: ResolvedConnectOptions;

  private constructor(target: SocketTarget, options?: ConnectOptions) {
    this.target = target;
    this.options = resolveConnectOptions(options);
  }

  public static unix(socketPath: string, options?: ConnectOptions): SocketTransport {
    return new SocketTransport({ kind: "unix", socketPath }, options);
  }

  public static tcp(
    hostContext: string,
    addr: { host: string; port: number },
    options?: ConnectOptions,
  ): SocketTransport {
    return new SocketTransport({ kind: "tcp", hostContext, host: addr.host, port: addr.port }, options);
  }

  public async control(): Promise<ControlChannel> {
    const socket = await this.connect();
    return new SocketControlChannel(socket);
  }

  public async raw(): Promise<RawDuplex> {
    const socket = await this.connect();
    return {
      readable: Readable.toWeb(socket) as ReadableStream<Uint8Array>,
      writable: Writable.toWeb(socket) as WritableStream<Uint8Array>,
      close: (): Promise<void> => {
        socket.end();
        socket.destroy();
        return Promise.resolve();
      },
    };
  }

  private connect(): Promise<Socket> {
    return new Promise((resolve, reject) => {
      const socket =
        this.target.kind === "unix"
          ? createConnection({ path: this.target.socketPath })
          : createConnection({ host: this.target.host, port: this.target.port });
      let settled = false;
      const timer = setTimeout(() => {
        fail(timeoutError(this.target.kind === "unix" ? "daemon socket connect" : "daemon tcp connect", this.options.connectTimeoutMs));
      }, this.options.connectTimeoutMs);

      const cleanup = (): void => {
        clearTimeout(timer);
        socket.off("connect", onConnect);
        socket.off("error", fail);
      };
      const onConnect = (): void => {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        resolve(socket);
      };
      const fail = (error: Error): void => {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        socket.destroy();
        reject(this.mapConnectError(error));
      };

      socket.once("connect", onConnect);
      socket.once("error", fail);
    });
  }

  private mapConnectError(error: Error): ClientError {
    if (this.target.kind === "unix") {
      return ClientError.daemonUnreachable(this.target.socketPath, error);
    }
    return ClientError.hostUnreachable(this.target.hostContext, error);
  }
}

class SocketControlChannel implements ControlChannel {
  public readonly lines: AsyncIterable<string>;
  private readonly socket: Socket;

  public constructor(socket: Socket) {
    this.socket = socket;
    this.lines = readControlLines(socket as AsyncIterable<Uint8Array>);
  }

  public async send(line: string): Promise<void> {
    const bytes = encodeControlLine(line);
    await writeBytes(this.socket, bytes);
  }

  public close(): Promise<void> {
    this.socket.end();
    this.socket.destroy();
    return Promise.resolve();
  }
}

function writeBytes(socket: Socket, bytes: Uint8Array): Promise<void> {
  if (socket.destroyed) {
    return Promise.reject(ClientError.io("socket is closed"));
  }

  return new Promise((resolve, reject) => {
    socket.write(bytes, (error) => {
      if (error instanceof Error) {
        reject(ClientError.io(error));
        return;
      }
      resolve();
    });
  });
}

function timeoutError(action: string, timeoutMs: number): Error {
  return new Error(`${action} timed out after ${timeoutMs}ms`);
}
