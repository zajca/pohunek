import { createConnection, type Socket } from "node:net";
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
    return socketRawDuplex(socket);
  }

  private connect(): Promise<Socket> {
    return new Promise((resolve, reject) => {
      // `allowHalfOpen` keeps the read side draining after the caller closes
      // the write side (attach can finish sending input while the daemon is
      // still streaming output); without it the socket auto-closes on the
      // peer's FIN and truncates an in-flight full-duplex round-trip.
      const socket =
        this.target.kind === "unix"
          ? createConnection({ path: this.target.socketPath, allowHalfOpen: true })
          : createConnection({ host: this.target.host, port: this.target.port, allowHalfOpen: true });
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

// Bridge a duplex `node:net` socket to a Web Streams `RawDuplex` for attach.
//
// The two halves are wired INDEPENDENTLY so a full-duplex round-trip is not
// truncated: closing the writable half only half-closes the socket (`end()`,
// sending FIN) and leaves the readable half draining the peer's remaining bytes
// until it observes `end`. `Readable.toWeb`/`Writable.toWeb` couple both halves
// to the same lifecycle and destroy the socket when the writable finishes,
// dropping in-flight inbound bytes — which a multi-megabyte round-trip exposes.
// Backpressure is honored in both directions: the readable pauses the socket
// once its queue fills and resumes on `pull`; the writable defers resolving a
// write until `drain` when the OS buffer is full.
function socketRawDuplex(socket: Socket): RawDuplex {
  let readerClosed = false;

  const readable = new ReadableStream<Uint8Array>({
    start(controller): void {
      socket.on("data", (chunk: Buffer): void => {
        if (readerClosed) {
          return;
        }
        controller.enqueue(new Uint8Array(chunk));
        if (controller.desiredSize !== null && controller.desiredSize <= 0) {
          socket.pause();
        }
      });
      socket.on("end", (): void => {
        if (!readerClosed) {
          readerClosed = true;
          controller.close();
        }
      });
      socket.on("error", (error: Error): void => {
        if (!readerClosed) {
          readerClosed = true;
          controller.error(ClientError.io(error));
        }
      });
    },
    pull(): void {
      socket.resume();
    },
    cancel(): void {
      readerClosed = true;
      socket.destroy();
    },
  });

  const writable = new WritableStream<Uint8Array>({
    write(chunk): Promise<void> {
      return new Promise((resolve, reject) => {
        if (socket.destroyed) {
          reject(ClientError.io("socket is closed"));
          return;
        }
        const flushed = socket.write(chunk, (error) => {
          if (error instanceof Error) {
            reject(ClientError.io(error));
          }
        });
        // Apply backpressure: only resolve once the OS buffer has drained, so
        // the producer cannot outrun the socket and buffer unboundedly.
        if (flushed) {
          resolve();
        } else {
          socket.once("drain", resolve);
        }
      });
    },
    close(): Promise<void> {
      // Intentionally do NOT `socket.end()` here. A half-close (FIN on the
      // write side only) is unreliable across runtimes: Bun does not honor
      // `allowHalfOpen`, so ending the write side also tears down the read
      // side and truncates in-flight daemon output. Closing the writable half
      // therefore just stops further writes; the underlying socket stays open
      // for reading until `RawDuplex.close()` (or the daemon) ends it. This
      // matches attach semantics: input ends, output keeps flowing until the
      // caller closes the stream or detaches on the control connection.
      return Promise.resolve();
    },
    abort(): void {
      socket.destroy();
    },
  });

  return {
    readable,
    writable,
    close: (): Promise<void> => {
      readerClosed = true;
      socket.destroy();
      return Promise.resolve();
    },
  };
}

function timeoutError(action: string, timeoutMs: number): Error {
  return new Error(`${action} timed out after ${timeoutMs}ms`);
}
