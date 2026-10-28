import {
  MAX_SESSION_WAIT_MS,
  MAX_SESSION_WAIT_TURN_MS,
  MAX_WORKER_INSTANCE_ID_BYTES,
  PROTOCOL_VERSION,
  CLIENT_PROTOCOL_VERSIONS,
  RANGE_FORWARDING_METHODS,
  MAX_COMMAND_TIMEOUT_MS,
  type CommandRunParams,
  type CommandRunResult,
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
import { CompatError, carriesSecretEnv, downgradeRequestParams, requestIsVersionIndependent, upgradeResult } from "./compat";
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

/** Why a control connection can no longer carry requests. */
interface Poison {
  readonly reason: string;
  /** Whether a fresh connection cures it; an incompatible version does not. */
  readonly recoverable: boolean;
}

/**
 * One control connection and everything learned on it. A request touches only
 * the connection it started on, so a late reply or timeout of an older
 * connection can never feed or poison a newer one.
 */
interface Connection {
  readonly channel: ControlChannel;
  readonly replies: AsyncIterator<string>;
  selectedVersion: ProtocolVersion | undefined;
  poisoned: Poison | undefined;
  /** `HostCapabilities` flags this connection's daemon advertised, learned by `host.inspect`. */
  verifiedCapabilities: Set<string>;
}

function openConnection(channel: ControlChannel): Connection {
  return {
    channel,
    replies: channel.lines[Symbol.asyncIterator](),
    selectedVersion: undefined,
    poisoned: undefined,
    verifiedCapabilities: new Set(),
  };
}

/** A replacement connection being opened for the poisoned connection `from`. */
interface Reopen {
  readonly from: Connection;
  promise: Promise<Connection>;
}

export class Client {
  private readonly transport: Transport;
  private connection: Connection;
  private reopening: Reopen | undefined;
  private readonly options: ResolvedConnectOptions;
  private readonly remoteHost: string | undefined;
  private consumed = false;
  private closed = false;

  private constructor(
    transport: Transport,
    channel: ControlChannel,
    options: ResolvedConnectOptions,
    remoteHost?: string,
  ) {
    this.transport = transport;
    this.connection = openConnection(channel);
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
      this.refuseSecretEnvOverRelay(creation);
      return this.callDedicated(
        "session.new",
        hasInitialInput(creation) ? { ...creation, extended_input_ready_wait: true } : creation,
        SESSION_NEW_REQUEST_BUDGET_MS
          + (creation.input_ack_timeout_ms === undefined
            ? 0
            : creation.input_ack_timeout_ms + MAX_SESSION_WAIT_MS),
      );
    }
    if (method === "command.run") {
      return this.runCommand(params as CommandRunParams);
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

  /**
   * Runs one package command on a dedicated connection whose deadline is the
   * wire timeout plus response headroom, so the budget of other calls is
   * unchanged. An omitted `timeout_ms` is bounded by `MAX_COMMAND_TIMEOUT_MS`,
   * the ceiling the daemon applies. A daemon that answers `method_not_found`, or
   * whose protocol version never defined the method, fails with
   * `command_unsupported`.
   */
  public async runCommand(params: CommandRunParams): Promise<CommandRunResult> {
    const wire = {
      package: params.package,
      command: params.command,
      ...(params.timeout_ms === undefined ? {} : { timeout_ms: params.timeout_ms }),
      ...(params.max_output_bytes === undefined ? {} : { max_output_bytes: params.max_output_bytes }),
    };
    const timeoutMs = Math.min(params.timeout_ms ?? MAX_COMMAND_TIMEOUT_MS, MAX_COMMAND_TIMEOUT_MS);
    try {
      return await this.callDedicated("command.run", wire, timeoutMs);
    } catch (error: unknown) {
      if (
        error instanceof ClientError
        && (error.kind === "daemonProtocolTooOld"
          || ((error.kind === "protocol" || error.kind === "remoteProtocol") && error.code === "method_not_found"))
      ) {
        throw ClientError.commandUnsupported(this.remoteHost);
      }
      throw error;
    }
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
      return validateInputWaitResult(wait, result);
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
    this.ensureOpen();
    // The request is copied before the first await: the caller keeps ownership
    // of its parameters and may change them once this call has returned a promise.
    const request = snapshotRequest(applyRequestOrigin(req, this.options.origin));
    omitDeclinedCommandsConsent(request);
    this.refuseSessionNewSecretEnvOverRelay(request);
    const connection = await this.usableConnection();
    return this.negotiateAndExchange(connection, request);
  }

  /** Applies the relay refusal to a prepared `session.new` request. */
  private refuseSessionNewSecretEnvOverRelay(request: Request): void {
    if (request.method === "session.new") {
      this.refuseSecretEnvOverRelay(request.params);
    }
  }

  /**
   * Refuses `secret_env` before a connection opens unless the transport states
   * it is not a relay; a transport that does not say is treated as one.
   */
  private refuseSecretEnvOverRelay(params: unknown): void {
    if (this.transport.relayed !== false && carriesSecretEnv(params)) {
      throw ClientError.sessionNewSecretEnvRelayRefused(this.remoteHost);
    }
  }

  /**
   * Lets a request that depends on a newer-daemon field reach the wire only on
   * a connection whose daemon advertised the capability flag; an older daemon
   * would drop the field silently or reject it. The check runs on the
   * connection that carries the request and is remembered for that connection
   * only.
   */
  private async requireCapability(connection: Connection, requirement: CapabilityRequirement): Promise<void> {
    if (connection.verifiedCapabilities.has(requirement.flag)) {
      return;
    }
    const inspect = applyRequestOrigin(
      {
        v: CLIENT_PROTOCOL_VERSIONS,
        id: nextRequestId("host.inspect"),
        method: "host.inspect",
        params: { skip_runtime_probes: true },
      },
      this.options.origin,
    );
    const capabilities = await withTimeout(
      this.exchange(connection, inspect, stringifyRequest(inspect)),
      this.options.requestTimeoutMs,
      () => {
        this.poison(connection, REQUEST_TIMED_OUT, true);
        return ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs);
      },
    );
    if ((capabilities as Record<string, unknown>)[requirement.flag] !== true) {
      throw requirement.unsupported(this.remoteHost);
    }
    connection.verifiedCapabilities.add(requirement.flag);
  }

  /**
   * Probes the version when the request needs it, then runs the exchange. The
   * probe and the exchange each get the request timeout, so a fresh connection
   * does not take its probe out of the caller's budget.
   */
  private async negotiateAndExchange(connection: Connection, request: Request): Promise<unknown> {
    if (request.method === "session.new" && carriesSecretEnv(request.params)) {
      if (this.transport.relayed !== false) {
        throw ClientError.sessionNewSecretEnvRelayRefused(this.remoteHost);
      }
      await this.requireCapability(connection, {
        flag: "session_new_secret_env_supported",
        unsupported: (host) => ClientError.sessionNewSecretEnvUnsupported(host),
      });
    }
    if (isPackageCommandsRequest(request)) {
      await this.requireCapability(connection, {
        flag: "package_install_commands_supported",
        unsupported: (host) => ClientError.packageInstallCommandsUnsupported(host),
      });
    }
    const prepared = await this.negotiatedRequest(connection, request);
    // The caller may have given up (timeout) or closed the client while the
    // version probe was in flight. A request must never be written after that.
    this.assertUsable(connection);
    return withTimeout(
      this.exchange(connection, prepared, stringifyRequest(prepared)),
      this.options.requestTimeoutMs,
      () => {
        this.poison(connection, REQUEST_TIMED_OUT, true);
        return ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs);
      },
    );
  }

  /**
   * Returns the connection the next request uses: the current one, or, when a
   * timeout or a broken reply poisoned it, a fresh one. Concurrent callers that
   * find the same poisoned connection share one replacement.
   */
  private async usableConnection(): Promise<Connection> {
    this.ensureOpen();
    const current = this.connection;
    const poison = current.poisoned;
    if (poison === undefined) {
      return current;
    }
    if (!poison.recoverable) {
      throw unusableError(poison);
    }
    if (this.reopening?.from === current) {
      return this.reopening.promise;
    }
    const attempt: Reopen = { from: current, promise: Promise.resolve(current) };
    attempt.promise = this.openReplacement(current).finally(() => {
      if (this.reopening === attempt) {
        this.reopening = undefined;
      }
    });
    this.reopening = attempt;
    return attempt.promise;
  }

  private async openReplacement(stale: Connection): Promise<Connection> {
    // The old connection is already known to be bad; its close neither fails
    // the call nor delays it past the connect timeout.
    await closeWithin(stale.channel, this.options.connectTimeoutMs);
    this.ensureOpen();
    const channel = await this.transport.control();
    if (this.closed) {
      await closeWithin(channel, this.options.connectTimeoutMs);
      throw ClientError.framing("connection is closed");
    }
    // The daemon may have restarted on another version, so nothing is inherited.
    const fresh = openConnection(channel);
    this.connection = fresh;
    return fresh;
  }

  /** Marks `connection` unusable; an unrecoverable mark is never softened. */
  private poison(connection: Connection, reason: string, recoverable: boolean): void {
    if (connection.poisoned?.recoverable === false) {
      return;
    }
    connection.poisoned = { reason, recoverable };
  }

  /**
   * Translates `request` into the version this connection speaks.
   *
   * The version probe offers only the versions the request itself allows, and a
   * selected version outside the request's own range is refused before anything
   * is rewritten or sent.
   */
  private async negotiatedRequest(connection: Connection, request: Request): Promise<Request> {
    const allowed = request.v;
    let version = connection.selectedVersion;
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
      version = await this.probeVersion(connection, probeRange);
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
  private async probeVersion(
    connection: Connection,
    range: ProtocolVersionRange,
  ): Promise<ProtocolVersion> {
    const probe = applyRequestOrigin(
      {
        v: range,
        id: nextRequestId("daemon.health"),
        method: "daemon.health",
        params: null,
      },
      this.options.origin,
    );
    await withTimeout(
      this.exchange(connection, probe, stringifyRequest(probe)),
      this.options.requestTimeoutMs,
      () => {
        this.poison(connection, REQUEST_TIMED_OUT, true);
        return ClientError.requestTimeout(this.remoteHost, this.options.requestTimeoutMs, "version probe");
      },
    );
    const version = connection.selectedVersion;
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
    this.ensureOpen();
    const prepared = snapshotRequest(applyRequestOrigin(request, this.options.origin));
    this.refuseSessionNewSecretEnvOverRelay(prepared);
    const connection = await this.usableConnection();
    try {
      await this.negotiateAndExchange(connection, prepared);
    } catch (error: unknown) {
      this.poison(connection, "subscription request failed; connection state is unknown", true);
      throw error;
    }

    this.consumed = true;
    const selectedVersion = connection.selectedVersion;
    if (selectedVersion === undefined) {
      throw ClientError.framing("subscription acknowledgement did not select a protocol version");
    }
    return new Subscription(connection.channel, selectedVersion, this.remoteHost);
  }

  public async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    await this.connection.channel.close();
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

  /** Refuses a client that was closed or whose subscription took the channel. */
  private ensureOpen(): void {
    if (this.closed) {
      throw ClientError.framing("connection is closed");
    }
    if (this.consumed) {
      throw ClientError.framing("connection is unusable: subscription consumed the control channel");
    }
  }

  /** Refuses a request to be written on a connection that went bad meanwhile. */
  private assertUsable(connection: Connection): void {
    this.ensureOpen();
    if (connection.poisoned !== undefined) {
      throw unusableError(connection.poisoned);
    }
  }

  private async exchange(connection: Connection, request: Request, line: string): Promise<unknown> {
    try {
      await connection.channel.send(line);
    } catch (error: unknown) {
      throw mapSendError(error);
    }

    const reply = await this.nextReply(connection);
    const response = this.parseResponse(connection, reply);
    if (response.id !== request.id) {
      this.poison(
        connection,
        `previous response id mismatch; expected '${request.id}', got '${response.id}'`,
        true,
      );
      throw responseIdMismatchError(this.remoteHost, request.id, response.id);
    }
    this.validateSelectedVersion(connection, request.v, response.v);

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
    connection: Connection,
    offered: ProtocolVersionRange,
    received: ProtocolVersion,
  ): void {
    const outsideOffered = received < offered.minimum || received > offered.maximum;
    const changed = connection.selectedVersion !== undefined && received !== connection.selectedVersion;
    if (outsideOffered || changed) {
      this.poison(connection, `response selected incompatible protocol version ${received}`, false);
      throw ClientError.versionMismatch(offered, received);
    }
    connection.selectedVersion = received;
  }

  private async callDedicated<
    K extends
      | "session.new"
      | "command.run"
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
    this.ensureOpen();
    const shared = this.connection;
    if (shared.poisoned?.recoverable === false) {
      throw unusableError(shared.poisoned);
    }
    // Parameters and the inherited version are captured before the first await.
    // A connection that went bad passes on nothing; the dedicated one probes.
    const snapshot = snapshotParams(params);
    const inherited = shared.poisoned === undefined ? shared.selectedVersion : undefined;
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
    client.connection.selectedVersion = inherited;
    try {
      this.ensureOpen();
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

  private async nextReply(connection: Connection): Promise<string> {
    try {
      const next = await connection.replies.next();
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

  private parseResponse(connection: Connection, reply: string): Response {
    let parsed: unknown;
    try {
      parsed = JSON.parse(reply) as unknown;
      return decodeResponse(parsed);
    } catch (error: unknown) {
      this.poison(connection, "previous daemon response was not valid JSON", true);
      if (this.remoteHost !== undefined) {
        throw ClientError.remoteDaemonUnavailable(this.remoteHost);
      }
      throw ClientError.json(error);
    }
  }
}

const REQUEST_TIMED_OUT = "previous request timed out; pending daemon response may be stale";

function unusableError(poison: Poison): ClientError {
  return ClientError.framing(`connection is unusable: ${poison.reason}`);
}

/**
 * Closes `channel` without waiting longer than `ms` for it. A broken WebSocket
 * can hang its close, and a close error is no news: the channel is abandoned.
 */
function closeWithin(channel: ControlChannel, ms: number): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    const done = (): void => {
      clearTimeout(timer);
      resolve();
    };
    try {
      channel.close().then(done, done);
    } catch {
      done();
    }
  });
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

/**
 * `accept_commands: false` is the default, and a daemon without package
 * commands rejects the key, so the client omits it from the wire.
 */
function omitDeclinedCommandsConsent(request: Request): void {
  if (!PACKAGE_COMMANDS_METHODS.has(request.method)) {
    return;
  }
  const params = request.params;
  if (typeof params === "object" && params !== null && !Array.isArray(params)
    && (params as { accept_commands?: unknown }).accept_commands === false) {
    delete (params as { accept_commands?: unknown }).accept_commands;
  }
}

/** A host capability a request depends on, and the error raised when the host lacks it. */
interface CapabilityRequirement {
  readonly flag: string;
  readonly unsupported: (host: string | undefined) => ClientError;
}

const PACKAGE_COMMANDS_METHODS: ReadonlySet<string> = new Set(["package.install", "package.link"]);

/** Whether `request` sets `accept_commands`, which only a daemon with package commands accepts. */
function isPackageCommandsRequest(request: Request): boolean {
  if (!PACKAGE_COMMANDS_METHODS.has(request.method)) {
    return false;
  }
  const params = request.params;
  return typeof params === "object" && params !== null && !Array.isArray(params)
    && (params as { accept_commands?: unknown }).accept_commands !== undefined;
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
  wait: SessionInputWait & { until: NonNullable<SessionInputWait["until"]> },
  value: unknown,
): SessionInputResult {
  const until = wait.until;
  if (typeof value !== "object" || value === null) {
    throw ClientError.inputWaitContract("the daemon returned a non-object result");
  }
  const result = value as Record<string, unknown>;
  if (result["accepted"] !== true) {
    throw ClientError.inputWaitContract("the daemon did not confirm accepted delivery");
  }
  const activity = result["activity"];
  if (activity === undefined && wait.allow_prompt_accepted === true) {
    return validatePromptOnlyResult(until, result, value as SessionInputResult);
  }
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

/**
 * A result without activity is valid only for a caller that opted in to a
 * prompt acceptance and asked for `working`, and only when it carries the
 * runtime and epoch with no partial activity evidence.
 */
function validatePromptOnlyResult(
  until: NonNullable<SessionInputWait["until"]>,
  result: Record<string, unknown>,
  value: SessionInputResult,
): SessionInputResult {
  if (result["prompt_accepted"] !== true) {
    throw ClientError.inputWaitContract(
      "the response activity did not match the requested wait target",
    );
  }
  if (!until.includes("working")) {
    throw ClientError.inputWaitContract(
      "a prompt acceptance does not satisfy the requested wait target",
    );
  }
  if (
    result["activity_source"] !== undefined
    || result["activity_revision"] !== undefined
    || !isRuntimeIdentity(result["runtime"])
    || typeof result["activity_epoch"] !== "string"
    || result["activity_epoch"].length === 0
  ) {
    throw ClientError.inputWaitContract(
      "the prompt acceptance response omitted its runtime and epoch or carried partial activity evidence",
    );
  }
  return value;
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
