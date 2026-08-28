import {
  MAX_SESSION_WAIT_MS,
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type Methods,
  type ProtocolError,
  type ProtocolVersion,
  type ProtocolVersionRange,
  type SessionOutputParams,
  type SessionOutputResult,
  type SessionInputParams,
  type SessionInputResult,
  type SessionInputWait,
  type SessionScreenParams,
  type SessionScreenResult,
  type StateSource,
  type SessionWaitParams,
  type SessionWaitResult,
} from "@pohunek/protocol";
import { ClientError } from "./error";
import { decodeResponse, type Request, type Response } from "./envelope";
import { hasValidWireOrigin, type RequestOrigin } from "./origin";
import { Subscription } from "./subscription";
import type { ConnectOptions, ControlChannel, ResolvedConnectOptions, Transport } from "./transport";
import { resolveConnectOptions } from "./transport";
import { WsTransport } from "./transport-ws";

// A 128-bit prefix keeps independently started SDK clients collision-resistant.
const RUN_TOKEN_RANDOM_BYTES = 16;
// Dedicated calls need response headroom beyond the daemon's overall wire deadline.
const DEDICATED_REQUEST_HEADROOM_MS = 1_000;
const MAX_U64 = 18_446_744_073_709_551_615n;
const STATE_SOURCES = new Set<StateSource>([
  "osc_title",
  "osc_progress",
  "screen",
  "process",
  "report",
]);
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
  private readonly transport: Transport;
  private readonly channel: ControlChannel;
  private readonly replies: AsyncIterator<string>;
  private readonly options: ResolvedConnectOptions;
  private readonly remoteHost: string | undefined;
  private poisoned: string | undefined;
  private consumed = false;
  private closed = false;
  private selectedVersion: ProtocolVersion | undefined;

  private constructor(
    transport: Transport,
    channel: ControlChannel,
    options: ResolvedConnectOptions,
    remoteHost?: string,
  ) {
    this.transport = transport;
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
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: nextRequestId(String(method)),
      method: String(method),
      params,
    };
    const value = await this.request(request);
    return value as Methods[K]["output"];
  }

  public async sessionScreen(params: SessionScreenParams): Promise<SessionScreenResult> {
    return this.call("session.screen", params);
  }

  public async sessionOutput(params: SessionOutputParams): Promise<SessionOutputResult> {
    if (params.wait_ms !== undefined) {
      return this.callDedicated("session.output", params, params.wait_ms);
    }
    return this.call("session.output", params);
  }

  public async sessionInput(params: SessionInputParams): Promise<SessionInputResult> {
    if (params.wait !== undefined) {
      const result = await this.callDedicated(
        "session.input",
        params,
        params.wait.timeout_ms ?? MAX_SESSION_WAIT_MS,
      );
      return validateInputWaitResult(params.wait.until, result);
    }
    return this.call("session.input", params);
  }

  public async sessionWait(params: SessionWaitParams): Promise<SessionWaitResult> {
    return this.callDedicated("session.wait", params, params.timeout_ms);
  }

  public async request(req: Request): Promise<unknown> {
    this.ensureUsable();
    const request = applyRequestOrigin(req, this.options.origin);

    let line: string;
    try {
      const encoded = JSON.stringify(request);
      if (encoded === undefined) {
        throw new Error("request serialized to undefined");
      }
      line = encoded;
    } catch (error: unknown) {
      throw ClientError.json(error);
    }

    try {
      return await withTimeout(
        this.exchange(request, line),
        this.options.requestTimeoutMs,
        () => {
          this.poisoned = "previous request timed out; pending daemon response may be stale";
          return ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs);
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
    const prepared = applyRequestOrigin(request, this.options.origin);
    try {
      await withTimeout(
        this.exchange(prepared, stringifyRequest(prepared)),
        this.options.requestTimeoutMs,
        () => ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs),
      );
    } catch (error: unknown) {
      this.poisoned = "subscription request failed; connection state is unknown";
      throw error;
    }

    this.consumed = true;
    const selectedVersion = this.selectedVersion;
    if (selectedVersion === undefined) {
      throw ClientError.framing("subscription acknowledgement did not select a protocol version");
    }
    return new Subscription(this.channel, selectedVersion, this.remoteHost);
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
    return new Client(transport, channel, options, remoteHost);
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

  private async exchange(request: Request, line: string): Promise<unknown> {
    try {
      await this.channel.send(line);
    } catch (error: unknown) {
      throw mapSendError(error);
    }

    const reply = await this.nextReply();
    const response = this.parseResponse(reply);
    if (response.id !== request.id) {
      this.poisoned = `previous response id mismatch; expected '${request.id}', got '${response.id}'`;
      throw responseIdMismatchError(this.remoteHost, request.id, response.id);
    }
    this.validateSelectedVersion(request.v, response.v);

    if ("ok" in response) {
      return response.ok;
    }
    throw mapDaemonError(this.remoteHost, response.err);
  }

  private validateSelectedVersion(
    offered: ProtocolVersionRange,
    received: ProtocolVersion,
  ): void {
    const outsideOffered = received < offered.minimum || received > offered.maximum;
    const changed = this.selectedVersion !== undefined && received !== this.selectedVersion;
    if (outsideOffered || changed) {
      this.poisoned = `response selected incompatible protocol version ${received}`;
      throw ClientError.versionMismatch(offered, received);
    }
    this.selectedVersion = received;
  }

  private async callDedicated<K extends "session.input" | "session.output" | "session.wait">(
    method: K,
    params: Methods[K]["params"],
    wireTimeoutMs: number,
  ): Promise<Methods[K]["output"]> {
    const requestTimeoutMs = Math.max(
      this.options.requestTimeoutMs,
      wireTimeoutMs + DEDICATED_REQUEST_HEADROOM_MS,
    );
    const client = await Client.connectTransport(
      this.transport,
      { ...this.options, requestTimeoutMs },
      this.remoteHost,
    );
    try {
      return await client.call(method, params);
    } finally {
      await client.close();
    }
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

function validateInputWaitResult(
  until: SessionInputWait["until"],
  value: unknown,
): SessionInputResult {
  if (typeof value !== "object" || value === null) {
    throw ClientError.inputWaitContract("the daemon returned a non-object result");
  }
  const result = value as Record<string, unknown>;
  if (result["accepted"] !== true) {
    throw ClientError.inputWaitContract("the daemon did not confirm accepted delivery");
  }
  const activity = result["activity"];
  const targets = until.length === 0 ? ["idle", "blocked"] : until;
  if (typeof activity !== "string" || !targets.includes(activity)) {
    throw ClientError.inputWaitContract(
      "the response activity did not match the requested wait target",
    );
  }
  if (!isStateSource(result["activity_source"])) {
    throw ClientError.inputWaitContract("the response omitted a valid activity source");
  }
  if (!isRuntimeIdentity(result["runtime"])) {
    throw ClientError.inputWaitContract("the response omitted a valid runtime identity");
  }
  if (typeof result["activity_epoch"] !== "string" || result["activity_epoch"].length === 0) {
    throw ClientError.inputWaitContract("the response omitted a valid activity epoch");
  }
  if (!isCanonicalDecimal(result["activity_revision"])) {
    throw ClientError.inputWaitContract("the response omitted a valid activity revision");
  }
  return value as SessionInputResult;
}

function isRuntimeIdentity(value: unknown): boolean {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const runtime = value as Record<string, unknown>;
  return typeof runtime["runtime_id"] === "string"
    && runtime["runtime_id"].length > 0
    && isCanonicalDecimal(runtime["runtime_generation"]);
}

function isStateSource(value: unknown): value is StateSource {
  return typeof value === "string" && STATE_SOURCES.has(value as StateSource);
}

function isCanonicalDecimal(value: unknown): boolean {
  return typeof value === "string"
    && /^(0|[1-9][0-9]*)$/.test(value)
    && BigInt(value) <= MAX_U64;
}

function applyRequestOrigin(request: Request, configured: RequestOrigin | undefined): Request {
  const wireSession = request.origin_session_id;
  const wireDaemon = request.origin_daemon_id;
  if (!hasValidWireOrigin(wireSession, wireDaemon)) {
    throw ClientError.framing("request origin markers must be a valid atomic pair");
  }
  if (configured === undefined) {
    return request;
  }
  if (
    wireSession !== undefined
    && (wireSession !== configured.sessionId || wireDaemon !== configured.daemonId)
  ) {
    throw ClientError.framing("request origin markers conflict with the client origin");
  }
  return {
    ...request,
    origin_session_id: configured.sessionId,
    origin_daemon_id: configured.daemonId,
  };
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
