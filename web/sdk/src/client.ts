import { PROTOCOL_VERSION, type Methods, type ProtocolError } from "@pohunek/protocol";
import { ClientError } from "./error";
import { decodeResponse, type Request, type Response } from "./envelope";
import { Subscription } from "./subscription";
import type { ConnectOptions, ControlChannel, ResolvedConnectOptions, Transport } from "./transport";
import { resolveConnectOptions } from "./transport";
import { WsTransport } from "./transport-ws";

// A 128-bit prefix keeps independently started SDK clients collision-resistant.
const RUN_TOKEN_RANDOM_BYTES = 16;
const RUN_TOKEN = `${randomToken()}${Date.now().toString(16)}`;
let nextSequence = 0;

function randomToken(): string {
  const bytes = new Uint8Array(RUN_TOKEN_RANDOM_BYTES);
  globalThis.crypto.getRandomValues(bytes);
  let token = "";
  for (const byte of bytes) {
    token += byte.toString(16).padStart(2, "0");
  }
  return token;
}

export class Client {
  private readonly channel: ControlChannel;
  private readonly replies: AsyncIterator<string>;
  private readonly options: ResolvedConnectOptions;
  private readonly remoteHost: string | undefined;
  private poisoned: string | undefined;
  private consumed = false;
  private closed = false;

  private constructor(channel: ControlChannel, options: ResolvedConnectOptions, remoteHost?: string) {
    this.channel = channel;
    this.replies = channel.lines[Symbol.asyncIterator]();
    this.options = options;
    this.remoteHost = remoteHost;
  }

  public static defaultOptions(): ResolvedConnectOptions {
    return resolveConnectOptions();
  }

  public static async connectWs(baseUrl: string, host: string, opts?: ConnectOptions): Promise<Client> {
    return Client.connectTransport(WsTransport.relay(baseUrl, host, opts), opts, host);
  }

  public async call<K extends keyof Methods>(
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]> {
    const request: Request = {
      v: PROTOCOL_VERSION,
      id: nextRequestId(String(method)),
      method: String(method),
      params,
    };
    const value = await this.request(request);
    return value as Methods[K]["output"];
  }

  public async request(req: Request): Promise<unknown> {
    this.ensureUsable();

    let line: string;
    try {
      const encoded = JSON.stringify(req);
      if (encoded === undefined) {
        throw new Error("request serialized to undefined");
      }
      line = encoded;
    } catch (error: unknown) {
      throw ClientError.json(error);
    }

    try {
      return await withTimeout(
        this.exchange(req.id, line),
        this.options.requestTimeoutMs,
        () => {
          this.poisoned = "previous request timed out; pending daemon response may be stale";
          return noResponseError(this.remoteHost, "timed out waiting for daemon response");
        },
      );
    } catch (error: unknown) {
      throw error;
    }
  }

  public async handshake(): Promise<number> {
    const result = await this.call("daemon.health", null);
    const daemonVersion = result.protocol_version;
    if (daemonVersion !== PROTOCOL_VERSION) {
      throw ClientError.versionMismatch(PROTOCOL_VERSION, daemonVersion);
    }
    return daemonVersion;
  }

  public async subscribe(request: Request): Promise<Subscription> {
    this.ensureUsable();
    try {
      await withTimeout(
        this.exchange(request.id, stringifyRequest(request)),
        this.options.requestTimeoutMs,
        () => noResponseError(this.remoteHost, "timed out waiting for subscription ack"),
      );
    } catch (error: unknown) {
      this.poisoned = "subscription request failed; connection state is unknown";
      throw error;
    }

    this.consumed = true;
    return new Subscription(this.channel, this.remoteHost);
  }

  public async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    await this.channel.close();
  }

  public static async connectTransport(
    transport: Transport,
    opts?: ConnectOptions,
    remoteHost?: string,
  ): Promise<Client> {
    const options = resolveConnectOptions(opts);
    const channel = await transport.control();
    return new Client(channel, options, remoteHost);
  }

  private ensureUsable(): void {
    if (this.closed) {
      throw ClientError.framing("connection is closed");
    }
    if (this.consumed) {
      throw ClientError.framing("connection is unusable: subscription consumed the control channel");
    }
    if (this.poisoned !== undefined) {
      throw ClientError.framing(`connection is unusable: ${this.poisoned}`);
    }
  }

  private async exchange(requestId: string, line: string): Promise<unknown> {
    try {
      await this.channel.send(line);
    } catch (error: unknown) {
      throw mapSendError(error);
    }

    const reply = await this.nextReply();
    const response = this.parseResponse(reply);
    if (response.id !== requestId) {
      this.poisoned = `previous response id mismatch; expected '${requestId}', got '${response.id}'`;
      throw responseIdMismatchError(this.remoteHost, requestId, response.id);
    }

    if ("ok" in response) {
      return response.ok;
    }
    throw mapDaemonError(this.remoteHost, response.err);
  }

  private async nextReply(): Promise<string> {
    try {
      const next = await this.replies.next();
      if (next.done === true) {
        throw noResponseError(this.remoteHost, "daemon closed the connection without a response");
      }
      return next.value;
    } catch (error: unknown) {
      if (error instanceof ClientError && error.kind === "framing" && this.remoteHost === undefined) {
        throw error;
      }
      if (this.remoteHost !== undefined) {
        throw ClientError.remoteDaemonUnavailable(this.remoteHost);
      }
      if (error instanceof ClientError) {
        throw error;
      }
      throw ClientError.io(error);
    }
  }

  private parseResponse(reply: string): Response {
    let parsed: unknown;
    try {
      parsed = JSON.parse(reply) as unknown;
      return decodeResponse(parsed);
    } catch (error: unknown) {
      this.poisoned = "previous daemon response was not valid JSON";
      if (this.remoteHost !== undefined) {
        throw ClientError.remoteDaemonUnavailable(this.remoteHost);
      }
      throw ClientError.json(error);
    }
  }
}

export function nextRequestId(method: string): string {
  const seq = nextSequence;
  nextSequence += 1;
  return `sdk-${method}-${RUN_TOKEN}-${seq}`;
}

function stringifyRequest(request: Request): string {
  try {
    const line = JSON.stringify(request);
    if (line === undefined) {
      throw new Error("request serialized to undefined");
    }
    return line;
  } catch (error: unknown) {
    throw ClientError.json(error);
  }
}

function mapDaemonError(remoteHost: string | undefined, error: ProtocolError): ClientError {
  if (remoteHost !== undefined) {
    return ClientError.remoteProtocol(remoteHost, error);
  }
  return ClientError.protocol(error);
}

function mapSendError(error: unknown): ClientError {
  if (error instanceof ClientError) {
    return error;
  }
  return ClientError.io(error);
}

function noResponseError(remoteHost: string | undefined, localMessage: string): ClientError {
  if (remoteHost !== undefined) {
    return ClientError.remoteDaemonUnavailable(remoteHost);
  }
  return ClientError.framing(localMessage);
}

function responseIdMismatchError(
  remoteHost: string | undefined,
  expected: string,
  actual: string,
): ClientError {
  if (remoteHost !== undefined) {
    return ClientError.remoteDaemonUnavailable(remoteHost);
  }
  return ClientError.framing(`response id mismatch: expected '${expected}', got '${actual}'`);
}

function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  onTimeout: () => ClientError,
): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(onTimeout());
    }, timeoutMs);

    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error instanceof Error ? error : ClientError.io(error));
      },
    );
  });
}
