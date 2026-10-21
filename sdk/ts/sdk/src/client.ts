import {
  MAX_SESSION_WAIT_MS,
  MAX_SESSION_WAIT_TURN_MS,
  MAX_WORKER_INSTANCE_ID_BYTES,
  PROTOCOL_VERSION,
  CLIENT_PROTOCOL_VERSIONS,
  RANGE_FORWARDING_METHODS,
  type Methods,
  type ProtocolVersion,
  type ProtocolVersionRange,
  type SessionOutputParams,
  type SessionOutputResult,
  type SessionInputParams,
  type SessionInputResult,
  type SessionInputWait,
  type SessionNewParams,
  type SessionScreenParams,
  type SessionScreenResult,
  type StateSource,
  type SessionWaitParams,
  type SessionWaitResult,
  type SessionWaitTurnParams,
  type SessionWaitTurnResult,
} from "@pohunek/protocol";
import { CompatError, downgradeRequestParams, requestIsVersionIndependent, upgradeResult } from "./compat";
import { ClientError, type LifecycleOperation, type ObservedProtocolError } from "./error";
import { decodeResponse, type Request, type Response } from "./envelope";
import { decodeHostGovernanceInspect } from "./governance";
import { hasValidWireOrigin, type RequestOrigin } from "./origin";
import { Subscription } from "./subscription";
import type { ConnectOptions, ControlChannel, ResolvedConnectOptions, Transport } from "./transport";
import {
  resolveConnectOptions,
  SESSION_FORK_REQUEST_BUDGET_MS,
  SESSION_NEW_REQUEST_BUDGET_MS,
  SESSION_REMOVE_REQUEST_BUDGET_MS,
  SESSION_RESUME_REQUEST_BUDGET_MS,
  SESSION_STOP_REQUEST_BUDGET_MS,
} from "./transport";
import { WsTransport } from "./transport-ws";

// A 128-bit prefix keeps independently started SDK clients collision-resistant.
const RUN_TOKEN_RANDOM_BYTES = 16;
// Dedicated calls need response headroom beyond the daemon's overall wire deadline.
const DEDICATED_REQUEST_HEADROOM_MS = 1_000;
const MAX_U64 = 18_446_744_073_709_551_615n;
// `timeout_ms` is a `u32` on the wire.
const MAX_U32 = 4_294_967_295;
const UTF8_ENCODER = new TextEncoder();
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
    if (method === "session.new") {
      const creation = params as SessionNewParams;
      return this.callDedicated(
        "session.new",
        hasInitialInput(creation) ? { ...creation, extended_input_ready_wait: true } : creation,
        SESSION_NEW_REQUEST_BUDGET_MS,
      );
    }
    if (method === "session.resume") {
      return this.callDedicated(
        "session.resume",
        params as Methods["session.resume"]["params"],
        SESSION_RESUME_REQUEST_BUDGET_MS,
      );
    }
    if (method === "session.fork") {
      return this.callDedicated(
        "session.fork",
        params as Methods["session.fork"]["params"],
        SESSION_FORK_REQUEST_BUDGET_MS,
      );
    }
    if (method === "session.stop") {
      return this.callDedicated(
        "session.stop",
        params as Methods["session.stop"]["params"],
        SESSION_STOP_REQUEST_BUDGET_MS,
      );
    }
    if (method === "session.remove" || method === "session.remove_accepting_unconfirmed") {
      return this.callDedicated(
        method,
        params as Methods["session.remove"]["params"],
        SESSION_REMOVE_REQUEST_BUDGET_MS,
      );
    }
    if (method === "session.input" && isWaitingSessionInput(params)) {
      return this.sessionInput(params);
    }
    if (method === "session.wait_turn") {
      return this.sessionWaitTurn(params as SessionWaitTurnParams);
    }
    if (method === "session.wait") {
      return this.sessionWait(params as SessionWaitParams);
    }
    if (method === "session.output") {
      return this.sessionOutput(params as SessionOutputParams);
    }
    return this.callDirect(method, params);
  }

  private async callDirect<K extends keyof Methods>(
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]> {
    const request: Request = {
      v: CLIENT_PROTOCOL_VERSIONS,
      id: nextRequestId(String(method)),
      method: String(method),
      params,
    };
    const value = await this.request(request);
    if (method === "host.governance.inspect") {
      return decodeHostGovernanceInspect(value);
    }
    return value as Methods[K]["output"];
  }

  public async sessionScreen(params: SessionScreenParams): Promise<SessionScreenResult> {
    return this.call("session.screen", params);
  }

  public async sessionOutput(params: SessionOutputParams): Promise<SessionOutputResult> {
    if (params.wait_ms !== undefined) {
      return this.callDedicated("session.output", params, params.wait_ms);
    }
    return this.callDirect("session.output", params);
  }

  public async sessionInput(params: SessionInputParams): Promise<SessionInputResult> {
    if (params.wait !== undefined) {
      const wait = {
        ...params.wait,
        until: [...(params.wait.until ?? [])],
      };
      const timeoutMs = validatedInputWaitTimeout(wait);
      const result = await this.callDedicated(
        "session.input",
        { ...params, wait },
        timeoutMs,
      );
      return validateInputWaitResult(wait.until, result);
    }
    return this.callDirect("session.input", params);
  }

  public async sessionWait(params: SessionWaitParams): Promise<SessionWaitResult> {
    return this.callDedicated("session.wait", params, params.timeout_ms);
  }

  /**
   * Waits for the next turn event of one or more sessions on a dedicated
   * connection.
   *
   * The connection deadline is `timeout_ms` plus response headroom. When the
   * request leaves the duration to the daemon default, the client cannot know
   * that default and bounds the connection by `MAX_SESSION_WAIT_TURN_MS`, the
   * ceiling no daemon setting may exceed. A `timeout_ms` that is not a nonzero
   * `u32` is refused before sending, because JSON would turn `NaN` or
   * `Infinity` into `null`, which the daemon reads as "use the default".
   * Against a daemon of the previous protocol version the call fails with
   * `daemon_protocol_too_old` before anything is sent.
   */
  public async sessionWaitTurn(params: SessionWaitTurnParams): Promise<SessionWaitTurnResult> {
    const timeoutMs = params.timeout_ms;
    if (timeoutMs !== undefined && !isNonzeroU32(timeoutMs)) {
      throw ClientError.protocol({
        class: "daemon",
        code: "bad_request",
        msg: "timeout_ms must be a nonzero u32 number of milliseconds",
      });
    }
    // A daemon refuses a duration above the protocol ceiling at once, so the
    // deadline never needs to exceed it.
    const wireTimeoutMs = Math.min(timeoutMs ?? MAX_SESSION_WAIT_TURN_MS, MAX_SESSION_WAIT_TURN_MS);
    return this.callDedicated("session.wait_turn", params, wireTimeoutMs);
  }

  /**
   * Sends one request and returns the `ok` payload in the current shape.
   *
   * When the daemon negotiated the previous protocol version the request is
   * translated into it before sending and the payload is translated back. A
   * request whose translation depends on the version first sends `daemon.health`
   * to learn it. A method the previous version never defined fails with a
   * `daemonProtocolTooOld` error before anything is sent.
   */
  public async request(req: Request): Promise<unknown> {
    this.ensureUsable();
    // The request is copied before the first await: the caller keeps ownership
    // of its parameters and may change them once this call has returned a promise.
    const request = snapshotRequest(applyRequestOrigin(req, this.options.origin));
    return withTimeout(
      this.negotiateAndExchange(request),
      this.options.requestTimeoutMs,
      () => {
        this.poisoned = "previous request timed out; pending daemon response may be stale";
        return ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs);
      },
    );
  }

  private async negotiateAndExchange(request: Request): Promise<unknown> {
    const prepared = await this.negotiatedRequest(request);
    // The caller may have given up (timeout) or closed the client while the
    // version probe was in flight. A request must never be written after that.
    this.ensureUsable();
    return this.exchange(prepared, stringifyRequest(prepared));
  }

  /**
   * Translates `request` into the version this connection speaks.
   *
   * The version probe offers only the versions the request itself allows, and a
   * selected version outside the request's own range is refused before anything
   * is rewritten or sent.
   */
  private async negotiatedRequest(request: Request): Promise<Request> {
    const allowed = request.v;
    let version = this.selectedVersion;
    if (version === undefined) {
      if (requestIsVersionIndependent(request.method, request.params)) {
        return request;
      }
      const probeRange = narrowToWindow(allowed);
      if (probeRange === undefined) {
        throw ClientError.protocol({
          class: "daemon",
          code: "version_mismatch",
          msg: `client protocol range ${allowed.minimum}..=${allowed.maximum} does not overlap daemon protocol range ${CLIENT_PROTOCOL_VERSIONS.minimum}..=${CLIENT_PROTOCOL_VERSIONS.maximum}`,
          recover: "upgrade the older side so the client and daemon support an overlapping protocol version",
        });
      }
      version = await this.probeVersion(probeRange);
    }
    if (version < allowed.minimum) {
      throw ClientError.daemonProtocolTooOld({
        host: this.remoteHost,
        method: request.method,
        daemonVersion: version,
        requiredVersion: allowed.minimum,
      });
    }
    if (version > allowed.maximum) {
      throw ClientError.versionMismatch(allowed, version);
    }
    // The request carries exactly the version the reply will be read with, so a
    // daemon that no longer speaks it refuses the request before executing.
    if (version === PROTOCOL_VERSION) {
      // A method that forwards the asking range keeps the client's whole window;
      // it is read-only, so there is no side effect to refuse.
      if ((RANGE_FORWARDING_METHODS as readonly string[]).includes(request.method)) {
        return request;
      }
      return { ...request, v: { minimum: version, maximum: version } };
    }
    try {
      return {
        ...request,
        v: { minimum: version, maximum: version },
        params: downgradeRequestParams(version, request.method, request.params),
      };
    } catch (error: unknown) {
      throw this.translationError(error, version, request.method);
    }
  }

  /** Learns the daemon's version with a `daemon.health` exchange offering `range`. */
  private async probeVersion(range: ProtocolVersionRange): Promise<ProtocolVersion> {
    const probe = applyRequestOrigin(
      {
        v: range,
        id: nextRequestId("daemon.health"),
        method: "daemon.health",
        params: null,
      },
      this.options.origin,
    );
    await this.exchange(probe, stringifyRequest(probe));
    const version = this.selectedVersion;
    if (version === undefined) {
      throw ClientError.framing("daemon health response did not select a protocol version");
    }
    return version;
  }

  private translationError(error: unknown, version: ProtocolVersion, method: string): unknown {
    if (!(error instanceof CompatError)) {
      return error;
    }
    if (error.failure.kind === "methodNotDefined") {
      return ClientError.daemonProtocolTooOld({
        host: this.remoteHost,
        method,
        daemonVersion: version,
        requiredVersion: PROTOCOL_VERSION,
      });
    }
    return ClientError.versionTranslation(this.remoteHost, version, error);
  }

  public async handshake(): Promise<number> {
    const result = await this.call("daemon.health", null);
    const daemonVersion = result.protocol_version;
    if (daemonVersion < CLIENT_PROTOCOL_VERSIONS.minimum || daemonVersion > CLIENT_PROTOCOL_VERSIONS.maximum) {
      throw ClientError.versionMismatch(CLIENT_PROTOCOL_VERSIONS, daemonVersion);
    }
    return daemonVersion;
  }

  public async subscribe(request: Request): Promise<Subscription> {
    this.ensureUsable();
    const prepared = snapshotRequest(applyRequestOrigin(request, this.options.origin));
    try {
      await withTimeout(
        this.negotiateAndExchange(prepared),
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
      if (response.v === PROTOCOL_VERSION) {
        return response.ok;
      }
      try {
        return upgradeResult(response.v, request.method, response.ok);
      } catch (error: unknown) {
        throw this.translationError(error, response.v, request.method);
      }
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

  private async callDedicated<
    K extends
      | "session.new"
      | "session.resume"
      | "session.fork"
      | "session.stop"
      | "session.remove"
      | "session.remove_accepting_unconfirmed"
      | "session.input"
      | "session.output"
      | "session.wait"
      | "session.wait_turn",
  >(
    method: K,
    params: Methods[K]["params"],
    wireTimeoutMs: number,
  ): Promise<Methods[K]["output"]> {
    this.ensureUsable();
    // Parameters and the inherited version are captured before the first await.
    const snapshot = snapshotParams(params);
    const inherited = this.selectedVersion;
    const lifecycle = LIFECYCLE_OPERATIONS.get(method);
    const requestTimeoutMs = isLifecycleMethod(method) && this.options.explicitRequestTimeoutMs !== undefined
      ? this.options.explicitRequestTimeoutMs
      : Math.max(this.options.requestTimeoutMs, wireTimeoutMs + DEDICATED_REQUEST_HEADROOM_MS);
    const client = await Client.connectTransport(
      this.transport,
      { ...this.options, requestTimeoutMs },
      this.remoteHost,
    );
    // The dedicated connection speaks the version this one already selected.
    client.selectedVersion = inherited;
    try {
      this.ensureUsable();
      return await client.callDirect(method, snapshot);
    } catch (error: unknown) {
      if (lifecycle !== undefined && error instanceof ClientError && error.kind === "requestTimeout") {
        throw ClientError.lifecycleOutcomeUnknown(lifecycle, this.remoteHost, requestTimeoutMs);
      }
      throw error;
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

/** Lifecycle requests that report an unknown outcome when their deadline elapses. */
const LIFECYCLE_OPERATIONS: ReadonlyMap<string, LifecycleOperation> = new Map([
  ["session.resume", "resume"],
  ["session.fork", "fork"],
  ["session.stop", "stop"],
  ["session.remove", "remove"],
  ["session.remove_accepting_unconfirmed", "remove"],
]);

/** Whether an explicit request timeout replaces the method's dedicated budget. */
function isLifecycleMethod(method: string): boolean {
  return method === "session.new" || LIFECYCLE_OPERATIONS.has(method);
}

/** Narrows `allowed` to the client window; `undefined` when they do not overlap. */
function narrowToWindow(allowed: ProtocolVersionRange): ProtocolVersionRange | undefined {
  const minimum = Math.max(allowed.minimum, CLIENT_PROTOCOL_VERSIONS.minimum);
  const maximum = Math.min(allowed.maximum, CLIENT_PROTOCOL_VERSIONS.maximum);
  return minimum > maximum ? undefined : { minimum, maximum };
}

function hasInitialInput(value: unknown): value is SessionNewParams & { input: string } {
  return typeof value === "object"
    && value !== null
    && "input" in value
    && typeof (value as { input?: unknown }).input === "string";
}

function isWaitingSessionInput(value: unknown): value is SessionInputParams & { wait: SessionInputWait } {
  return typeof value === "object"
    && value !== null
    && "wait" in value
    && (value as { wait?: unknown }).wait !== undefined;
}

function isNonzeroU32(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= MAX_U32;
}

function validatedInputWaitTimeout(wait: SessionInputWait): number {
  const timeoutMs = wait.timeout_ms ?? MAX_SESSION_WAIT_MS;
  if (!Number.isInteger(timeoutMs) || timeoutMs <= 0) {
    throw ClientError.protocol({
      class: "runtime",
      code: "session_input_invalid_wait",
      msg: "timeout_ms must be greater than zero",
    });
  }
  if (timeoutMs > MAX_SESSION_WAIT_MS) {
    throw ClientError.protocol({
      class: "runtime",
      code: "session_wait_limit_exceeded",
      msg: "the requested wait exceeds the configured maximum",
    });
  }
  return timeoutMs;
}

function validateInputWaitResult(
  until: NonNullable<SessionInputWait["until"]>,
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
  return typeof runtime["worker_instance_id"] === "string"
    && runtime["worker_instance_id"].length > 0
    && UTF8_ENCODER.encode(runtime["worker_instance_id"]).byteLength <= MAX_WORKER_INSTANCE_ID_BYTES
    && !/\p{Cc}/u.test(runtime["worker_instance_id"])
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

/** Deep copy of the request exactly as it would be serialized. */
function snapshotRequest(request: Request): Request {
  return JSON.parse(stringifyRequest(request)) as Request;
}

function snapshotParams<T>(params: T): T {
  if (params === undefined) {
    return params;
  }
  try {
    return JSON.parse(JSON.stringify(params)) as T;
  } catch (error: unknown) {
    throw ClientError.json(error);
  }
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

function mapDaemonError(remoteHost: string | undefined, error: ObservedProtocolError): ClientError {
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
