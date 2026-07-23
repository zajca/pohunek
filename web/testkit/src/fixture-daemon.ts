import { Buffer } from "node:buffer";
import { unlink } from "node:fs/promises";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import {
  EVENT_AGENT_STATE,
  EVENT_ATTACH_CLOSED,
  EVENT_ATTACH_OPENED,
  EVENT_NOTIFICATION_CREATED,
  EVENT_NOTIFICATION_DELETED,
  EVENT_NOTIFICATION_UPDATED,
  EVENT_SESSION_CREATED,
  EVENT_SESSION_REMOVED,
  EVENT_SESSION_STOPPED,
  EVENT_SESSION_UPDATED,
  MAX_CONTROL_LINE_BYTES,
  PROTOCOL_VERSION,
  type AgentActivity,
  type AgentKind,
  type ErrorClass,
  type HostCapabilities,
  type HostRecord,
  type Methods,
  type NotificationCreateParams,
  type NotificationCreateResult,
  type NotificationId,
  type NotificationListParams,
  type NotificationListResult,
  type NotificationRecord,
  type NotificationSeverity,
  type NotificationStatus,
  type NotificationUpdateParams,
  type NotificationUpdateResult,
  type ProjectAddParams,
  type ProjectInfo,
  type ProjectRemoveParams,
  type ProjectShowParams,
  type ProjectWorktree,
  type SessionForkParams,
  type ProtocolError,
  type ProtocolEvent,
  type ProtocolVersion,
  type SessionAttachParams,
  type SessionAttachResult,
  type SessionDetachParams,
  type SessionDetachResult,
  type SessionInfo,
  type SessionListFilter,
  type SessionListParams,
  type SessionNewParams,
  type SessionNewResult,
  type SessionResizeParams,
  type SessionResizeResult,
  type SessionRenameParams,
  type SessionSetMetadataParams,
  type SessionStopResult,
  type WorktreeRemoveParams,
  type StateSource,
} from "@pohunek/protocol";
import { ActivePtyAttach, FixturePtyRegistry, type FixturePtyEvents, type FixturePtyOptions } from "./pty";
import {
  FixtureScenario,
  type ScenarioBackend,
  type ScenarioNotificationInput,
  type ScenarioResize,
} from "./scenario";

export type FixtureDaemonEndpoint =
  | { readonly kind: "unix"; readonly socketPath: string }
  | { readonly kind: "tcp"; readonly host: string; readonly port: number };

export interface FixtureDaemonListenOptions {
  readonly unixSocketPath?: string;
  readonly tcp?: {
    readonly host: string;
    readonly port: number;
  };
}

export interface FixtureHostOptions {
  readonly capabilities?: HostCapabilities;
  readonly discoveredHosts?: readonly HostRecord[];
}

export interface StartFixtureDaemonOptions {
  readonly listen: FixtureDaemonListenOptions;
  readonly daemonVersion?: string;
  /** Protocol version reported by `daemon.health`; framing still uses the generated protocol version. */
  readonly protocolVersion?: ProtocolVersion;
  readonly host?: FixtureHostOptions;
  readonly initialSessions?: readonly SessionInfo[];
  readonly initialNotifications?: readonly NotificationRecord[];
  readonly initialProjects?: readonly FixtureProject[];
  readonly pty?: FixturePtyOptions;
}

export interface FixtureProject {
  readonly project: ProjectInfo;
  readonly worktrees?: readonly ProjectWorktree[];
}

export interface FixtureDaemonHandle {
  readonly endpoints: readonly FixtureDaemonEndpoint[];
  readonly unixSocketPath: string | undefined;
  readonly tcpAddress: { readonly host: string; readonly port: number } | undefined;
  readonly scenario: FixtureScenario;
  close(): Promise<void>;
  stopAbruptly(): Promise<void>;
}

type MethodName = keyof Methods;
type JsonRecord = Record<string, unknown>;

interface ControlRequest {
  readonly v: number;
  readonly id: string;
  readonly method: string;
  readonly params: unknown;
}

type ControlResponse =
  | { readonly v: ProtocolVersion; readonly id: string; readonly ok: unknown }
  | { readonly v: ProtocolVersion; readonly id: string; readonly err: ProtocolError };

interface SocketContext {
  activeAttach?: ActivePtyAttach;
  mode: "line" | "raw" | "subscribed";
  pendingLineChunks: Uint8Array[];
  pendingLineBytes: number;
  queue: Promise<void>;
}

const DEFAULT_DAEMON_VERSION = "0.0.0-testkit";
const DEFAULT_CWD = "/tmp/pohunek-testkit";
const FIRST_FIXTURE_PID = 42_000;
const SESSION_ID_PREFIX = "s-testkit-";
const NOTIFICATION_ID_PREFIX = "n-testkit-";
const PROJECT_ID_PREFIX = "p-testkit-";
const LINE_FEED = 0x0a;
const CARRIAGE_RETURN = 0x0d;
const SUPPORTED_AGENTS = ["shell", "codex", "claude"] as const;
const SESSION_STATES = ["starting", "running", "stopped", "done", "failed"] as const;
const AGENT_ACTIVITIES = ["working", "blocked", "idle"] as const;
const NOTIFICATION_KINDS = [
  "agent_blocked",
  "approval_required",
  "turn_completed",
  "session_finished",
  "error",
  "system",
] as const;
const NOTIFICATION_SEVERITIES = ["info", "success", "warning", "error", "action_required"] as const;
const NOTIFICATION_STATUSES = ["unread", "read", "acknowledged", "archived", "deleted"] as const;
const DEFAULT_NOTIFICATION_SOURCE = {
  provider: "pohunek-testkit",
  provider_event: "scenario",
  host_local_source_id: "testkit-scenario",
} as const;

const decoder = new TextDecoder("utf-8", { fatal: true });

class FixtureDaemon implements FixtureDaemonHandle, FixturePtyEvents, ScenarioBackend {
  public readonly scenario: FixtureScenario;

  private readonly listenOptions: FixtureDaemonListenOptions;
  private readonly daemonVersion: string;
  private readonly protocolVersion: ProtocolVersion;
  private readonly hostCapabilities: HostCapabilities;
  private readonly pty: FixturePtyRegistry;
  private readonly servers: Server[] = [];
  private readonly sockets = new Set<Socket>();
  private readonly subscribers = new Set<Socket>();
  private readonly sessions = new Map<string, SessionInfo>();
  private readonly notifications = new Map<string, NotificationRecord>();
  private readonly projects = new Map<string, ProjectInfo>();
  private readonly projectWorktrees = new Map<string, ProjectWorktree[]>();
  private readonly sessionResizes = new Map<string, ScenarioResize[]>();
  private endpointsValue: FixtureDaemonEndpoint[] = [];
  private discoveredHosts: HostRecord[];
  private nextSessionId = 1;
  private nextNotificationId = 1;
  private nextPid = FIRST_FIXTURE_PID;
  private closed = false;

  public constructor(options: StartFixtureDaemonOptions) {
    assertListenOptions(options.listen);
    this.listenOptions = options.listen;
    this.daemonVersion = options.daemonVersion ?? DEFAULT_DAEMON_VERSION;
    this.protocolVersion = options.protocolVersion ?? PROTOCOL_VERSION;
    this.hostCapabilities = cloneValue(options.host?.capabilities ?? defaultHostCapabilities(this.daemonVersion));
    this.discoveredHosts = cloneValue([...(options.host?.discoveredHosts ?? [])]);
    this.pty = new FixturePtyRegistry(this, options.pty);
    this.scenario = new FixtureScenario(this);

    for (const session of options.initialSessions ?? []) {
      const cloned = cloneValue(session);
      this.sessions.set(cloned.id, cloned);
      this.nextPid = Math.max(this.nextPid, cloned.pid + 1);
    }
    for (const notification of options.initialNotifications ?? []) {
      const cloned = cloneValue(notification);
      this.notifications.set(cloned.id, cloned);
    }
    for (const fixture of options.initialProjects ?? []) {
      const project = cloneValue(fixture.project);
      this.projects.set(project.id, project);
      this.projectWorktrees.set(project.id, cloneValue([...(fixture.worktrees ?? [])]));
    }
  }

  public get endpoints(): readonly FixtureDaemonEndpoint[] {
    return this.endpointsValue;
  }

  public get unixSocketPath(): string | undefined {
    const endpoint = this.endpointsValue.find((candidate) => candidate.kind === "unix");
    return endpoint?.kind === "unix" ? endpoint.socketPath : undefined;
  }

  public get tcpAddress(): { readonly host: string; readonly port: number } | undefined {
    const endpoint = this.endpointsValue.find((candidate) => candidate.kind === "tcp");
    if (endpoint?.kind !== "tcp") {
      return undefined;
    }
    return { host: endpoint.host, port: endpoint.port };
  }

  public async start(): Promise<FixtureDaemonHandle> {
    if (this.listenOptions.unixSocketPath !== undefined) {
      await this.listenUnix(this.listenOptions.unixSocketPath);
    }
    if (this.listenOptions.tcp !== undefined) {
      await this.listenTcp(this.listenOptions.tcp.host, this.listenOptions.tcp.port);
    }
    return this;
  }

  public async close(): Promise<void> {
    await this.shutdown();
  }

  public async stopAbruptly(): Promise<void> {
    await this.shutdown();
  }

  public emitAttachOpened(sessionId: string, streamId: string): void {
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_ATTACH_OPENED,
      session_id: sessionId,
      stream_id: streamId,
    });
  }

  public emitAttachClosed(sessionId: string, streamId: string): void {
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_ATTACH_CLOSED,
      session_id: sessionId,
      stream_id: streamId,
    });
  }

  public setAgentState(sessionId: string, activity: AgentActivity, source: StateSource): void {
    const session = this.requireSession(sessionId);
    session.activity = activity;
    session.state_source = source;
    session.updated_at = timestamp();
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_AGENT_STATE,
      session_id: sessionId,
      activity,
      source,
    });
  }

  public removeSession(sessionId: string): void {
    const session = this.requireSession(sessionId);
    this.sessions.delete(sessionId);
    this.pty.closeSession(sessionId);
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_SESSION_REMOVED,
      session: cloneValue(session),
    });
  }

  public createScenarioNotification(input: ScenarioNotificationInput): NotificationRecord {
    return this.createNotification(input, true);
  }

  public deleteNotification(id: NotificationId): void {
    if (!this.notifications.delete(id)) {
      throw new Error(`unknown fixture notification: ${id}`);
    }
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_NOTIFICATION_DELETED,
      notification_id: id,
    });
  }

  public resizes(sessionId: string): ReadonlyArray<ScenarioResize> {
    return (this.sessionResizes.get(sessionId) ?? []).map((resize) => ({ ...resize }));
  }

  public writeToPty(sessionId: string, bytes: Uint8Array): number {
    this.requireSession(sessionId);
    return this.pty.writeToSession(sessionId, bytes);
  }

  public queuePtyOutput(sessionId: string, bytes: Uint8Array): void {
    this.requireSession(sessionId);
    this.pty.queueOutput(sessionId, bytes);
  }

  public setDiscoveredHosts(hosts: readonly HostRecord[]): void {
    this.discoveredHosts = cloneValue([...hosts]);
  }

  private async listenUnix(socketPath: string): Promise<void> {
    const server = createServer((socket) => {
      this.serveSocket(socket);
    });
    await listen(server, socketPath);
    this.servers.push(server);
    this.endpointsValue = [...this.endpointsValue, { kind: "unix", socketPath }];
  }

  private async listenTcp(host: string, port: number): Promise<void> {
    const server = createServer((socket) => {
      this.serveSocket(socket);
    });
    await listen(server, { host, port });
    const address = server.address();
    if (!isAddressInfo(address)) {
      throw new Error("fixture TCP listener did not report an address");
    }
    this.servers.push(server);
    this.endpointsValue = [...this.endpointsValue, { kind: "tcp", host, port: address.port }];
  }

  private serveSocket(socket: Socket): void {
    const context: SocketContext = {
      mode: "line",
      pendingLineChunks: [],
      pendingLineBytes: 0,
      queue: Promise.resolve(),
    };
    this.sockets.add(socket);

    socket.on("data", (chunk: Buffer): void => {
      this.handleSocketData(socket, context, chunk);
    });
    socket.once("end", (): void => {
      this.flushPendingLine(socket, context);
    });
    socket.once("close", (): void => {
      this.sockets.delete(socket);
      this.subscribers.delete(socket);
      context.activeAttach?.finish();
    });
    socket.once("error", (): void => {
      this.sockets.delete(socket);
      this.subscribers.delete(socket);
      context.activeAttach?.finish();
    });
  }

  private handleSocketData(socket: Socket, context: SocketContext, chunk: Buffer): void {
    if (context.mode === "raw") {
      context.activeAttach?.writeInput(new Uint8Array(chunk));
      return;
    }
    if (context.mode === "subscribed") {
      socket.destroy();
      return;
    }

    let offset = 0;
    while (offset < chunk.byteLength) {
      const newlineIndex = chunk.indexOf(LINE_FEED, offset);
      const segmentEnd = newlineIndex < 0 ? chunk.byteLength : newlineIndex;
      if (segmentEnd > offset && !appendPendingLine(context, chunk.subarray(offset, segmentEnd))) {
        socket.destroy();
        return;
      }
      if (newlineIndex < 0) {
        return;
      }

      const line = decodeControlLine(takePendingLine(context));
      if (line === undefined) {
        this.writeResponse(socket, errResponse("", badRequest("invalid utf-8 control line")));
        offset = newlineIndex + 1;
        continue;
      }

      const attachStreamId = parseAttachPrelude(line.trim());
      if (attachStreamId !== undefined) {
        const bufferedInput = new Uint8Array(chunk.subarray(newlineIndex + 1));
        const redeemed = this.redeemAttach(socket, attachStreamId, bufferedInput);
        context.mode = "raw";
        if (redeemed !== undefined) {
          context.activeAttach = redeemed;
        }
        return;
      }

      this.enqueueControlLine(socket, context, line);
      offset = newlineIndex + 1;
    }
  }

  private flushPendingLine(socket: Socket, context: SocketContext): void {
    if (context.mode !== "line" || context.pendingLineBytes === 0) {
      return;
    }
    const line = decodeControlLine(takePendingLine(context));
    if (line === undefined) {
      this.writeResponse(socket, errResponse("", badRequest("invalid utf-8 control line")));
      return;
    }
    this.enqueueControlLine(socket, context, line);
  }

  private enqueueControlLine(socket: Socket, context: SocketContext, line: string): void {
    context.queue = context.queue
      .then(() => this.handleControlLine(socket, context, line))
      .catch((error: unknown) => {
        socket.destroy(error instanceof Error ? error : undefined);
      });
  }

  private handleControlLine(socket: Socket, context: SocketContext, line: string): void {
    const trimmed = line.trim();
    if (trimmed.length === 0) {
      this.writeResponse(socket, errResponse("", badRequest("empty request line")));
      return;
    }

    const request = parseRequest(trimmed);
    if ("err" in request) {
      this.writeResponse(socket, errResponse("", request.err));
      return;
    }

    if (request.v !== PROTOCOL_VERSION) {
      this.writeResponse(socket, errResponse(request.id, versionMismatch(request.v)));
      return;
    }

    if (request.method === "subscribe") {
      this.subscribers.add(socket);
      context.mode = "subscribed";
      this.writeResponse(socket, okResponse(request.id, { subscribed: true }));
      return;
    }

    this.writeResponse(socket, this.dispatchRequest(request));
  }

  private dispatchRequest(request: ControlRequest): ControlResponse {
    switch (request.method as MethodName) {
      case "daemon.health":
        return okResponse(request.id, {
          status: "ok",
          daemon_version: this.daemonVersion,
          protocol_version: this.protocolVersion,
        } satisfies Methods["daemon.health"]["output"]);
      case "host.inspect":
        return okResponse(request.id, cloneValue(this.hostCapabilities));
      case "host.discover":
        return this.handleHostDiscover(request);
      case "session.new":
        return this.handleSessionNew(request);
      case "session.list":
        return this.handleSessionList(request);
      case "session.inspect":
        return this.handleSessionInspect(request);
      case "session.stop":
        return this.handleSessionStop(request);
      case "session.rename":
        return this.handleSessionRename(request);
      case "session.set_metadata":
        return this.handleSessionSetMetadata(request);
      case "session.resume":
        return this.handleSessionResume(request);
      case "session.fork":
        return this.handleSessionFork(request);
      case "session.remove":
        return this.handleSessionRemove(request);
      case "session.attach":
        return this.handleSessionAttach(request);
      case "session.detach":
        return this.handleSessionDetach(request);
      case "session.resize":
        return this.handleSessionResize(request);
      case "notification.list":
        return this.handleNotificationList(request);
      case "notification.update":
        return this.handleNotificationUpdate(request);
      case "notification.create":
        return this.handleNotificationCreate(request);
      case "project.list":
        return this.handleProjectList(request);
      case "project.add":
        return this.handleProjectAdd(request);
      case "project.show":
        return this.handleProjectShow(request);
      case "project.rename":
        return this.handleProjectRename(request);
      case "project.remove":
        return this.handleProjectRemove(request);
      case "worktree.remove":
        return this.handleWorktreeRemove(request);
      default:
        return errResponse(request.id, methodNotFound(request.method));
    }
  }

  private handleHostDiscover(request: ControlRequest): ControlResponse {
    if (!isHostDiscoverParams(request.params)) {
      return errResponse(request.id, invalidParams(request.method));
    }
    return okResponse(request.id, cloneValue(this.discoveredHosts));
  }

  private handleSessionNew(request: ControlRequest): ControlResponse {
    const params = readSessionNewParams(request);
    if (params === undefined) {
      return errResponse(request.id, invalidParams(request.method));
    }

    const session = this.buildSession(params);
    this.sessions.set(session.id, session);
    this.emitSessionEvent(EVENT_SESSION_CREATED, session);

    const result: SessionNewResult = cloneValue(session);
    if (params.input !== undefined) {
      result.applied_input = true;
    }
    return okResponse(request.id, result);
  }

  private handleSessionList(request: ControlRequest): ControlResponse {
    const params = readOptionalObject<SessionListParams>(request);
    if (params === undefined || !isSessionListParams(params)) {
      return errResponse(request.id, invalidParams(request.method));
    }

    let sessions = Array.from(this.sessions.values());
    for (const filter of params.filters ?? []) {
      sessions = sessions.filter((session) => sessionMatchesFilter(session, filter));
    }
    return okResponse(request.id, sessions.map((session) => cloneValue(session)));
  }

  private handleSessionInspect(request: ControlRequest): ControlResponse {
    if (typeof request.params !== "string") {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(request.params);
    if (session === undefined) {
      return errResponse(request.id, sessionNotFound(request.params));
    }
    return okResponse(request.id, cloneValue(session));
  }

  private handleSessionStop(request: ControlRequest): ControlResponse {
    if (typeof request.params !== "string") {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(request.params);
    if (session === undefined) {
      return errResponse(request.id, sessionNotFound(request.params));
    }
    if (session.state !== "running" && session.state !== "starting") {
      return okResponse(request.id, { stopped: false } satisfies SessionStopResult);
    }

    session.state = "stopped";
    session.updated_at = timestamp();
    session.exit_code = 0;
    this.pty.closeSession(session.id);
    this.emitSessionEvent(EVENT_SESSION_STOPPED, session);
    return okResponse(request.id, { stopped: true } satisfies SessionStopResult);
  }

  private handleSessionRename(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionRenameParams>(request);
    if (params === undefined || typeof params.session_id !== "string" || (params.name !== undefined && typeof params.name !== "string")) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(params.session_id);
    if (session === undefined) return errResponse(request.id, sessionNotFound(params.session_id));
    if (params.name === undefined) delete session.name;
    else {
      const name = params.name.trim();
      if (name.length === 0) return errResponse(request.id, badRequest("session name must not be empty"));
      session.name = name;
    }
    session.updated_at = timestamp();
    this.emitSessionEvent(EVENT_SESSION_UPDATED, session);
    return okResponse(request.id, { session: cloneValue(session) });
  }

  private handleSessionSetMetadata(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionSetMetadataParams>(request);
    if (params === undefined || typeof params.session_id !== "string" || !isStringOrNullRecord(params.metadata)) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(params.session_id);
    if (session === undefined) return errResponse(request.id, sessionNotFound(params.session_id));
    const metadata = { ...(session.metadata ?? {}) };
    for (const [key, value] of Object.entries(params.metadata)) {
      if (value === null) delete metadata[key]; else metadata[key] = value;
    }
    if (Object.keys(metadata).length === 0) delete session.metadata; else session.metadata = metadata;
    session.updated_at = timestamp();
    this.emitSessionEvent(EVENT_SESSION_UPDATED, session);
    return okResponse(request.id, { session: cloneValue(session) });
  }

  private handleSessionResume(request: ControlRequest): ControlResponse {
    if (typeof request.params !== "string") return errResponse(request.id, invalidParams(request.method));
    const session = this.sessions.get(request.params);
    if (session === undefined) return errResponse(request.id, sessionNotFound(request.params));
    if (session.state === "running" || session.external === true) return errResponse(request.id, badRequest("session cannot be resumed"));
    session.state = "running";
    delete session.exit_code;
    session.updated_at = timestamp();
    this.emitSessionEvent(EVENT_SESSION_UPDATED, session);
    return okResponse(request.id, { session: cloneValue(session) });
  }

  private handleSessionFork(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionForkParams>(request);
    if (params === undefined || typeof params.session_id !== "string" || params.cwd_mode !== "same" || !isPositiveInteger(params.cols) || !isPositiveInteger(params.rows)) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const source = this.sessions.get(params.session_id);
    if (source === undefined) return errResponse(request.id, sessionNotFound(params.session_id));
    if (source.external === true) return errResponse(request.id, badRequest("external sessions cannot be forked"));
    const session = this.buildSession({ agent: source.agent, cols: params.cols, rows: params.rows, cwd: source.cwd, ...(params.name === undefined ? {} : { name: params.name }) });
    if (source.metadata !== undefined) session.metadata = cloneValue(source.metadata);
    this.sessions.set(session.id, session);
    this.emitSessionEvent(EVENT_SESSION_CREATED, session);
    return okResponse(request.id, cloneValue(session));
  }

  private handleSessionRemove(request: ControlRequest): ControlResponse {
    if (typeof request.params !== "string") return errResponse(request.id, invalidParams(request.method));
    const session = this.sessions.get(request.params);
    if (session === undefined) return okResponse(request.id, { removed: false, stopped: false });
    if (session.external === true) return errResponse(request.id, badRequest("external sessions cannot be removed"));
    const stopped = session.state === "running" || session.state === "starting";
    this.sessions.delete(session.id);
    this.pty.closeSession(session.id);
    this.emitEvent({ v: PROTOCOL_VERSION, event: EVENT_SESSION_REMOVED, session: cloneValue(session) });
    return okResponse(request.id, { removed: true, stopped });
  }

  private handleSessionAttach(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionAttachParams>(request);
    if (params === undefined || typeof params.session_id !== "string") {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(params.session_id);
    if (session === undefined) {
      return errResponse(request.id, sessionNotFound(params.session_id));
    }
    if (session.state !== "running") {
      return errResponse(request.id, sessionNotRunning(params.session_id));
    }

    const streamId = this.pty.mint(params.session_id);
    return okResponse(request.id, { stream_id: streamId } satisfies SessionAttachResult);
  }

  private handleSessionDetach(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionDetachParams>(request);
    if (params === undefined || typeof params.stream_id !== "string") {
      return errResponse(request.id, invalidParams(request.method));
    }
    const detached = this.pty.detach(params.stream_id);
    return okResponse(request.id, { detached } satisfies SessionDetachResult);
  }

  private handleSessionResize(request: ControlRequest): ControlResponse {
    const params = readObjectParams<SessionResizeParams>(request);
    if (
      params === undefined ||
      typeof params.session_id !== "string" ||
      !isPositiveInteger(params.cols) ||
      !isPositiveInteger(params.rows)
    ) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const session = this.sessions.get(params.session_id);
    if (session === undefined) {
      return errResponse(request.id, sessionNotFound(params.session_id));
    }
    if (session.state !== "running") {
      return errResponse(request.id, sessionNotRunning(params.session_id));
    }
    session.cols = params.cols;
    session.rows = params.rows;
    session.updated_at = timestamp();
    const resize = { cols: params.cols, rows: params.rows } satisfies ScenarioResize;
    const resizes = this.sessionResizes.get(params.session_id);
    if (resizes === undefined) {
      this.sessionResizes.set(params.session_id, [resize]);
    } else {
      resizes.push(resize);
    }
    this.emitSessionEvent(EVENT_SESSION_UPDATED, session);
    return okResponse(request.id, { session: cloneValue(session) } satisfies SessionResizeResult);
  }

  private handleNotificationCreate(request: ControlRequest): ControlResponse {
    const params = readObjectParams<NotificationCreateParams>(request);
    if (params === undefined || !isNotificationCreateParams(params)) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const record = this.createNotification(params, true);
    return okResponse(request.id, { created: true, record } satisfies NotificationCreateResult);
  }

  private handleNotificationList(request: ControlRequest): ControlResponse {
    const params = readOptionalObject<NotificationListParams>(request);
    if (params === undefined || !isNotificationListParams(params)) {
      return errResponse(request.id, invalidParams(request.method));
    }

    const cursor = parseCursor(params.cursor);
    if (cursor === undefined) {
      return errResponse(request.id, invalidNotificationCursor(String(params.cursor)));
    }

    const limit = params.limit ?? Number.POSITIVE_INFINITY;
    const filtered = Array.from(this.notifications.values())
      .filter((record) => notificationMatches(record, params))
      .sort(compareNotifications);
    const page = filtered.slice(cursor, cursor + limit);
    const result: NotificationListResult = {
      notifications: page.map((record) => cloneValue(record)),
    };
    if (cursor + limit < filtered.length) {
      result.next_cursor = String(cursor + limit);
    }
    return okResponse(request.id, result);
  }

  private handleNotificationUpdate(request: ControlRequest): ControlResponse {
    const params = readObjectParams<NotificationUpdateParams>(request);
    if (params === undefined || typeof params.id !== "string" || !isNotificationStatus(params.status)) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const record = this.notifications.get(params.id);
    if (record === undefined) {
      return errResponse(request.id, notificationNotFound(params.id));
    }
    if (!canTransitionNotification(record.status, params.status)) {
      return errResponse(request.id, invalidNotificationTransition());
    }

    applyNotificationStatus(record, params.status);
    this.emitNotificationUpdated(record);
    return okResponse(request.id, { record: cloneValue(record) } satisfies NotificationUpdateResult);
  }

  private handleProjectList(request: ControlRequest): ControlResponse {
    if (!isOptionalEmptyObject(request.params)) return errResponse(request.id, invalidParams(request.method));
    return okResponse(request.id, Array.from(this.projects.values()).map((project) => cloneValue(project)));
  }

  private handleProjectAdd(request: ControlRequest): ControlResponse {
    const params = readObjectParams<ProjectAddParams>(request);
    if (params === undefined || typeof params.path !== "string" || !params.path.startsWith("/") || (params.name !== undefined && typeof params.name !== "string") || (params.base_branch !== undefined && typeof params.base_branch !== "string")) {
      return errResponse(request.id, invalidParams(request.method));
    }
    const now = timestamp();
    const id = `${PROJECT_ID_PREFIX}${this.projects.size + 1}`;
    const project: ProjectInfo = {
      id,
      label: normalizedProjectLabel(params.name, params.path),
      repo_root: params.path,
      git_common_dir: `${params.path}/.git`,
      ...(params.base_branch === undefined ? {} : { default_base_branch: params.base_branch }),
      source: "manual",
      is_bare: false,
      added_at: now,
      last_used_at: now,
    };
    this.projects.set(id, project);
    this.projectWorktrees.set(id, [{ path: params.path, ...(params.base_branch === undefined ? {} : { branch: params.base_branch }), head: "testkit-head", bare: false, locked: false, owned: false }]);
    return okResponse(request.id, cloneValue(project));
  }

  private handleProjectShow(request: ControlRequest): ControlResponse {
    const params = readObjectParams<ProjectShowParams>(request);
    if (params === undefined || typeof params.reference !== "string") return errResponse(request.id, invalidParams(request.method));
    const project = this.projectByReference(params.reference);
    if (project === undefined) return errResponse(request.id, badRequest("project was not found"));
    return okResponse(request.id, { project: cloneValue(project), worktrees: cloneValue(this.projectWorktrees.get(project.id) ?? []) });
  }

  private handleProjectRename(request: ControlRequest): ControlResponse {
    const params = readObjectParams<Methods["project.rename"]["params"]>(request);
    if (params === undefined || typeof params.reference !== "string" || typeof params.name !== "string" || params.name.trim().length === 0) return errResponse(request.id, invalidParams(request.method));
    const project = this.projectByReference(params.reference);
    if (project === undefined) return errResponse(request.id, badRequest("project was not found"));
    project.label = params.name.trim();
    return okResponse(request.id, cloneValue(project));
  }

  private handleProjectRemove(request: ControlRequest): ControlResponse {
    const params = readObjectParams<ProjectRemoveParams>(request);
    if (params === undefined || typeof params.reference !== "string" || typeof params.prune_worktrees !== "boolean") return errResponse(request.id, invalidParams(request.method));
    const project = this.projectByReference(params.reference);
    if (project === undefined) return okResponse(request.id, { removed: false, pruned_worktrees: 0 });
    const worktrees = this.projectWorktrees.get(project.id) ?? [];
    const removable = params.prune_worktrees ? worktrees.filter((worktree) => worktree.owned && worktree.session_id === undefined) : [];
    const skipped = params.prune_worktrees ? worktrees.filter((worktree) => worktree.owned && worktree.session_id !== undefined).map((worktree) => worktree.session_id as string) : [];
    this.projects.delete(project.id);
    this.projectWorktrees.delete(project.id);
    return okResponse(request.id, { removed: true, pruned_worktrees: removable.length, ...(skipped.length > 0 ? { skipped_worktrees: skipped } : {}) });
  }

  private handleWorktreeRemove(request: ControlRequest): ControlResponse {
    const params = readObjectParams<WorktreeRemoveParams>(request);
    if (params === undefined || typeof params.path !== "string") return errResponse(request.id, invalidParams(request.method));
    for (const [projectId, worktrees] of this.projectWorktrees) {
      const index = worktrees.findIndex((worktree) => worktree.path === params.path);
      if (index < 0) continue;
      const worktree = worktrees[index];
      if (worktree === undefined) continue;
      if (!worktree.owned || worktree.session_id !== undefined) return errResponse(request.id, badRequest("worktree is not removable"));
      worktrees.splice(index, 1);
      this.projectWorktrees.set(projectId, worktrees);
      return okResponse(request.id, { removed: true });
    }
    return okResponse(request.id, { removed: false });
  }

  private projectByReference(reference: string): ProjectInfo | undefined {
    return Array.from(this.projects.values()).find((project) => project.id === reference || project.label === reference);
  }

  private buildSession(params: SessionNewParams): SessionInfo {
    const now = timestamp();
    const id = `${SESSION_ID_PREFIX}${this.nextSessionId}`;
    this.nextSessionId += 1;

    const session: SessionInfo = {
      id,
      agent: params.agent,
      agent_base: agentBaseFor(params.agent),
      cwd: params.cwd ?? DEFAULT_CWD,
      cwd_source: "launch",
      pid: this.nextPid,
      cols: params.cols,
      rows: params.rows,
      state: "running",
      state_source: "process",
      activity: "idle",
      created_at: now,
      updated_at: now,
    };
    this.nextPid += 1;

    if (params.name !== undefined) {
      const name = params.name.trim();
      if (name.length > 0) {
        session.name = name;
      }
    }
    if (params.metadata !== undefined) {
      session.metadata = cloneValue(params.metadata);
    }
    if (params.project !== undefined) {
      session.project_id = params.project;
    }
    if (params.repo !== undefined) {
      session.repo = params.repo;
    }
    if (params.branch !== undefined) {
      session.branch = params.branch;
      session.is_linked_worktree = params.repo !== undefined;
      session.worktree_path = `${session.cwd}/.pohunek-worktrees/${params.branch}`;
    }

    return session;
  }

  private createNotification(input: ScenarioNotificationInput, emit: boolean): NotificationRecord {
    const id = input.id ?? `${NOTIFICATION_ID_PREFIX}${this.nextNotificationId}`;
    this.nextNotificationId += 1;
    const record: NotificationRecord = {
      id,
      source: cloneValue(input.source ?? DEFAULT_NOTIFICATION_SOURCE),
      kind: input.kind,
      severity: input.severity,
      status: input.status ?? "unread",
      title: input.title,
      body: input.body,
      created_at: input.created_at ?? timestamp(),
    };

    copyNotificationOptionals(record, input);
    this.notifications.set(record.id, record);
    if (emit) {
      this.emitEvent({
        v: PROTOCOL_VERSION,
        event: EVENT_NOTIFICATION_CREATED,
        record: cloneValue(record),
      });
    }
    return cloneValue(record);
  }

  private redeemAttach(socket: Socket, streamId: string, bufferedInput: Uint8Array): ActivePtyAttach | undefined {
    const redeemed = this.pty.redeem(streamId, socket, bufferedInput);
    if (redeemed === undefined) {
      this.writeResponse(socket, errResponse(streamId, attachNotFound(streamId)));
      socket.end();
      return undefined;
    }
    return redeemed.active;
  }

  private emitSessionEvent(event: "session_created" | "session_updated" | "session_stopped", session: SessionInfo): void {
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event,
      session: cloneValue(session),
    });
  }

  private emitNotificationUpdated(record: NotificationRecord): void {
    this.emitEvent({
      v: PROTOCOL_VERSION,
      event: EVENT_NOTIFICATION_UPDATED,
      record: cloneValue(record),
    });
  }

  private emitEvent(event: ProtocolEvent): void {
    const line = JSON.stringify(event);
    if (line === undefined) {
      return;
    }
    for (const subscriber of this.subscribers) {
      writeLine(subscriber, line);
    }
  }

  private writeResponse(socket: Socket, response: ControlResponse): void {
    const line = JSON.stringify(response);
    if (line === undefined) {
      return;
    }
    writeLine(socket, line);
  }

  private requireSession(sessionId: string): SessionInfo {
    const session = this.sessions.get(sessionId);
    if (session === undefined) {
      throw new Error(`unknown fixture session: ${sessionId}`);
    }
    return session;
  }

  private async shutdown(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;

    this.subscribers.clear();
    this.pty.closeAll();
    for (const socket of this.sockets) {
      socket.destroy();
    }
    this.sockets.clear();

    await Promise.all(this.servers.map((server) => closeServer(server)));
    await Promise.all(
      this.endpointsValue
        .filter((endpoint): endpoint is Extract<FixtureDaemonEndpoint, { kind: "unix" }> => endpoint.kind === "unix")
        .map((endpoint) => unlinkSocket(endpoint.socketPath)),
    );
  }
}

export async function startFixtureDaemon(options: StartFixtureDaemonOptions): Promise<FixtureDaemonHandle> {
  const daemon = new FixtureDaemon(options);
  return daemon.start();
}

function assertListenOptions(listenOptions: FixtureDaemonListenOptions): void {
  if (listenOptions.unixSocketPath === undefined && listenOptions.tcp === undefined) {
    throw new Error("fixture daemon requires at least one Unix or TCP listener");
  }
  if (listenOptions.unixSocketPath !== undefined && listenOptions.unixSocketPath.length === 0) {
    throw new Error("fixture daemon Unix socket path must not be empty");
  }
  if (listenOptions.tcp !== undefined) {
    if (listenOptions.tcp.host.length === 0) {
      throw new Error("fixture daemon TCP host must not be empty");
    }
    if (!Number.isInteger(listenOptions.tcp.port) || listenOptions.tcp.port < 0) {
      throw new Error("fixture daemon TCP port must be a non-negative integer");
    }
  }
}

function defaultHostCapabilities(daemonVersion: string): HostCapabilities {
  return {
    daemon_version: daemonVersion,
    protocol_version: PROTOCOL_VERSION,
    supported_agents: [...SUPPORTED_AGENTS],
    runtimes: SUPPORTED_AGENTS.map((agent) => ({ agent, available: true })),
    git_available: true,
    worktree_supported: true,
  };
}

function listen(server: Server, target: string | { readonly host: string; readonly port: number }): Promise<void> {
  return new Promise((resolve, reject) => {
    const fail = (error: Error): void => {
      server.off("listening", done);
      reject(error);
    };
    const done = (): void => {
      server.off("error", fail);
      resolve();
    };
    server.once("error", fail);
    server.once("listening", done);
    if (typeof target === "string") {
      server.listen(target);
    } else {
      server.listen(target.port, target.host);
    }
  });
}

function closeServer(server: Server): Promise<void> {
  if (!server.listening) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    server.close((error?: Error): void => {
      if (error !== undefined) {
        reject(error);
        return;
      }
      resolve();
    });
  });
}

async function unlinkSocket(socketPath: string): Promise<void> {
  try {
    await unlink(socketPath);
  } catch (error: unknown) {
    if (!isErrnoException(error) || error.code !== "ENOENT") {
      throw error;
    }
  }
}

function isAddressInfo(value: string | AddressInfo | null): value is AddressInfo {
  return typeof value === "object" && value !== null;
}

function parseAttachPrelude(line: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line) as unknown;
  } catch {
    return undefined;
  }
  if (!isRecord(parsed) || Object.keys(parsed).length !== 1) {
    return undefined;
  }
  const attach = parsed["attach"];
  return typeof attach === "string" && attach.length > 0 ? attach : undefined;
}

function parseRequest(line: string): ControlRequest | { readonly err: ProtocolError } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line) as unknown;
  } catch (error: unknown) {
    return { err: badRequest(`invalid request JSON: ${messageFromUnknown(error)}`) };
  }
  if (!isRecord(parsed)) {
    return { err: badRequest("invalid request envelope") };
  }

  const version = parsed["v"];
  const id = parsed["id"];
  const method = parsed["method"];
  if (typeof version !== "number" || typeof id !== "string" || typeof method !== "string") {
    return { err: badRequest("invalid request envelope") };
  }
  return {
    v: version,
    id,
    method,
    params: Object.hasOwn(parsed, "params") ? parsed["params"] : null,
  };
}

function appendPendingLine(context: SocketContext, bytes: Uint8Array): boolean {
  context.pendingLineBytes += bytes.byteLength;
  if (context.pendingLineBytes > MAX_CONTROL_LINE_BYTES) {
    return false;
  }
  context.pendingLineChunks.push(bytes);
  return true;
}

function takePendingLine(context: SocketContext): Uint8Array {
  if (context.pendingLineChunks.length === 1) {
    const [only] = context.pendingLineChunks;
    context.pendingLineChunks = [];
    context.pendingLineBytes = 0;
    return only ?? new Uint8Array();
  }

  const line = new Uint8Array(context.pendingLineBytes);
  let offset = 0;
  for (const chunk of context.pendingLineChunks) {
    line.set(chunk, offset);
    offset += chunk.byteLength;
  }
  context.pendingLineChunks = [];
  context.pendingLineBytes = 0;
  return line;
}

function decodeControlLine(bytes: Uint8Array): string | undefined {
  const trimmed = bytes.at(-1) === CARRIAGE_RETURN ? bytes.subarray(0, bytes.byteLength - 1) : bytes;
  try {
    return decoder.decode(trimmed);
  } catch {
    return undefined;
  }
}

function readObjectParams<T extends object>(request: ControlRequest): T | undefined {
  if (!isRecord(request.params)) {
    return undefined;
  }
  return request.params as T;
}

function readOptionalObject<T extends object>(request: ControlRequest): T | undefined {
  if (request.params === null) {
    return {} as T;
  }
  return readObjectParams<T>(request);
}

function readSessionNewParams(request: ControlRequest): SessionNewParams | undefined {
  const params = readObjectParams<SessionNewParams & JsonRecord>(request);
  if (
    params === undefined ||
    typeof params.agent !== "string" ||
    !isPositiveInteger(params.cols) ||
    !isPositiveInteger(params.rows)
  ) {
    return undefined;
  }
  if (!optionalString(params.name) || !optionalString(params.cwd) || !optionalString(params.project)) {
    return undefined;
  }
  if (!optionalString(params.repo) || !optionalString(params.branch) || !optionalString(params.base_branch)) {
    return undefined;
  }
  if (!optionalString(params.input) || !optionalStringRecord(params.metadata)) {
    return undefined;
  }
  const hasRepo = params.repo !== undefined;
  const hasBranch = params.branch !== undefined;
  if (hasRepo !== hasBranch) {
    return undefined;
  }
  if (params.base_branch !== undefined && (!hasRepo || !hasBranch)) {
    return undefined;
  }
  return params;
}

function isHostDiscoverParams(value: unknown): boolean {
  if (value === null) {
    return true;
  }
  if (!isRecord(value)) {
    return false;
  }
  return value["force"] === undefined || typeof value["force"] === "boolean";
}

function isSessionListParams(value: SessionListParams): boolean {
  const filters = value.filters;
  if (filters === undefined) {
    return true;
  }
  if (!Array.isArray(filters)) {
    return false;
  }
  return filters.every(isSessionListFilter);
}

function isSessionListFilter(value: unknown): value is SessionListFilter {
  if (!isRecord(value) || typeof value["key"] !== "string") {
    return false;
  }
  const filterValue = value["value"];
  switch (value["key"]) {
    case "state":
      return isSessionState(filterValue);
    case "activity":
      return isAgentActivity(filterValue);
    case "agent":
    case "id":
    case "project":
      return typeof filterValue === "string";
    default:
      return false;
  }
}

function sessionMatchesFilter(session: SessionInfo, filter: SessionListFilter): boolean {
  switch (filter.key) {
    case "state":
      return session.state === filter.value;
    case "activity":
      return session.activity === filter.value;
    case "agent":
      return session.agent === filter.value;
    case "id":
      return session.id === filter.value;
    case "project":
      return session.project_id === filter.value || session.project_label === filter.value;
  }
}

function isNotificationCreateParams(value: NotificationCreateParams): boolean {
  return (
    isNotificationSource(value.source) &&
    isNotificationKind(value.kind) &&
    isNotificationSeverity(value.severity) &&
    typeof value.title === "string" &&
    typeof value.body === "string" &&
    optionalStringRecord(value.metadata) &&
    optionalString(value.session_id) &&
    optionalAgentKind(value.agent_kind) &&
    optionalString(value.source_id) &&
    optionalString(value.dedupe_key) &&
    optionalString(value.project_id)
  );
}

function isNotificationListParams(value: NotificationListParams): boolean {
  return (
    optionalNotificationStatus(value.status) &&
    optionalNotificationKind(value.kind) &&
    optionalNotificationSeverity(value.severity) &&
    optionalString(value.provider) &&
    optionalString(value.session_id) &&
    optionalString(value.created_after) &&
    optionalString(value.created_before) &&
    optionalPositiveInteger(value.limit) &&
    optionalString(value.cursor)
  );
}

function notificationMatches(record: NotificationRecord, params: NotificationListParams): boolean {
  if (params.status === undefined && record.status === "deleted") {
    return false;
  }
  if (params.status !== undefined && record.status !== params.status) {
    return false;
  }
  if (params.kind !== undefined && record.kind !== params.kind) {
    return false;
  }
  if (params.severity !== undefined && record.severity !== params.severity) {
    return false;
  }
  if (params.provider !== undefined && record.source.provider !== params.provider) {
    return false;
  }
  if (params.session_id !== undefined && record.session_id !== params.session_id) {
    return false;
  }
  if (params.created_after !== undefined && record.created_at < params.created_after) {
    return false;
  }
  if (params.created_before !== undefined && record.created_at >= params.created_before) {
    return false;
  }
  return true;
}

function compareNotifications(left: NotificationRecord, right: NotificationRecord): number {
  const byCreatedAt = right.created_at.localeCompare(left.created_at);
  if (byCreatedAt !== 0) {
    return byCreatedAt;
  }
  return left.id.localeCompare(right.id);
}

function parseCursor(cursor: string | undefined): number | undefined {
  if (cursor === undefined) {
    return 0;
  }
  const parsed = Number(cursor);
  if (!Number.isInteger(parsed) || parsed < 0) {
    return undefined;
  }
  return parsed;
}

function canTransitionNotification(from: NotificationStatus, to: NotificationStatus): boolean {
  if (from === to) {
    return true;
  }
  if (from === "deleted") {
    return false;
  }
  switch (to) {
    case "read":
      return from === "unread";
    case "acknowledged":
      return from === "unread" || from === "read";
    case "archived":
      return from === "unread" || from === "read" || from === "acknowledged";
    case "deleted":
      return true;
    case "unread":
      return false;
  }
}

function applyNotificationStatus(record: NotificationRecord, status: NotificationStatus): void {
  record.status = status;
  const now = timestamp();
  if (status === "read") {
    record.read_at = now;
  }
  if (status === "acknowledged") {
    record.acked_at = now;
  }
  if (status === "archived") {
    record.archived_at = now;
  }
  if (status === "deleted") {
    record.deleted_at = now;
  }
}

function copyNotificationOptionals(record: NotificationRecord, input: ScenarioNotificationInput): void {
  if (input.metadata !== undefined) {
    record.metadata = cloneValue(input.metadata);
  }
  if (input.session_id !== undefined) {
    record.session_id = input.session_id;
  }
  if (input.agent_kind !== undefined) {
    record.agent_kind = input.agent_kind;
  }
  if (input.source_id !== undefined) {
    record.source_id = input.source_id;
  }
  if (input.dedupe_key !== undefined) {
    record.dedupe_key = input.dedupe_key;
  }
  if (input.project_id !== undefined) {
    record.project_id = input.project_id;
  }
  if (input.read_at !== undefined) {
    record.read_at = input.read_at;
  }
  if (input.acked_at !== undefined) {
    record.acked_at = input.acked_at;
  }
  if (input.archived_at !== undefined) {
    record.archived_at = input.archived_at;
  }
  if (input.deleted_at !== undefined) {
    record.deleted_at = input.deleted_at;
  }
  if (input.superseded_by !== undefined) {
    record.superseded_by = input.superseded_by;
  }
}

function okResponse(id: string, ok: unknown): ControlResponse {
  return { v: PROTOCOL_VERSION, id, ok };
}

function errResponse(id: string, err: ProtocolError): ControlResponse {
  return { v: PROTOCOL_VERSION, id, err };
}

function methodNotFound(method: string): ProtocolError {
  return protocolError("daemon", "method_not_found", `unknown control method: ${method}`);
}

function badRequest(message: string): ProtocolError {
  return protocolError("daemon", "bad_request", message);
}

function versionMismatch(clientVersion: number): ProtocolError {
  return protocolError(
    "daemon",
    "version_mismatch",
    `client protocol version ${clientVersion} is incompatible with daemon protocol version ${PROTOCOL_VERSION}`,
    "upgrade the older side so both speak the same protocol version",
  );
}

function sessionNotFound(sessionId: string): ProtocolError {
  return protocolError("runtime", "session_not_found", `session not found: ${sessionId}`);
}

function sessionNotRunning(sessionId: string): ProtocolError {
  return protocolError("runtime", "session_not_running", `session is not running: ${sessionId}`);
}

function attachNotFound(streamId: string): ProtocolError {
  return protocolError("runtime", "attach_not_found", `attach stream is not available: ${streamId}`);
}

function notificationNotFound(notificationId: string): ProtocolError {
  return protocolError("runtime", "notification_not_found", `notification not found: ${notificationId}`);
}

function invalidNotificationTransition(): ProtocolError {
  return protocolError(
    "runtime",
    "invalid_notification_transition",
    "invalid notification status transition",
    "use an allowed transition: unread->read, read->acknowledged, unread->acknowledged, archive a non-deleted record, or delete a non-deleted record",
  );
}

function invalidNotificationCursor(cursor: string): ProtocolError {
  return protocolError("runtime", "invalid_notification_cursor", `invalid notification cursor: ${cursor}`);
}

function invalidParams(method: string): ProtocolError {
  return badRequest(`invalid params for ${method}`);
}

function protocolError(errorClass: ErrorClass, code: string, msg: string, recover?: string): ProtocolError {
  if (recover === undefined) {
    return { class: errorClass, code, msg };
  }
  return { class: errorClass, code, msg, recover };
}

function agentBaseFor(agent: string): AgentKind {
  if (agent === "codex") {
    return "codex";
  }
  if (agent === "claude") {
    return "claude";
  }
  return "shell";
}

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isPositiveInteger(value: unknown): value is number {
  return Number.isInteger(value) && typeof value === "number" && value > 0;
}

function optionalPositiveInteger(value: unknown): boolean {
  return value === undefined || isPositiveInteger(value);
}

function optionalString(value: unknown): boolean {
  return value === undefined || typeof value === "string";
}

function optionalStringRecord(value: unknown): boolean {
  return value === undefined || isStringRecord(value);
}

function optionalAgentKind(value: unknown): boolean {
  return value === undefined || isAgentKind(value);
}

function optionalNotificationStatus(value: unknown): boolean {
  return value === undefined || isNotificationStatus(value);
}

function optionalNotificationKind(value: unknown): boolean {
  return value === undefined || isNotificationKind(value);
}

function optionalNotificationSeverity(value: unknown): boolean {
  return value === undefined || isNotificationSeverity(value);
}

function isStringRecord(value: unknown): value is Record<string, string> {
  if (!isRecord(value)) {
    return false;
  }
  return Object.values(value).every((entry) => typeof entry === "string");
}

function isStringOrNullRecord(value: unknown): value is Record<string, string | null> {
  return isRecord(value) && Object.values(value).every((entry) => typeof entry === "string" || entry === null);
}

function isOptionalEmptyObject(value: unknown): boolean {
  return value === null || (isRecord(value) && Object.keys(value).length === 0);
}

function normalizedProjectLabel(name: string | undefined, path: string): string {
  const label = name?.trim();
  if (label !== undefined && label.length > 0) return label;
  return path.split("/").filter((segment) => segment.length > 0).at(-1) ?? path;
}

function isNotificationSource(value: unknown): value is NotificationCreateParams["source"] {
  return (
    isRecord(value) &&
    typeof value["provider"] === "string" &&
    typeof value["provider_event"] === "string" &&
    typeof value["host_local_source_id"] === "string"
  );
}

function isAgentKind(value: unknown): value is AgentKind {
  return value === "shell" || value === "codex" || value === "claude";
}

function isSessionState(value: unknown): value is SessionInfo["state"] {
  return (SESSION_STATES as readonly unknown[]).includes(value);
}

function isAgentActivity(value: unknown): value is AgentActivity {
  return (AGENT_ACTIVITIES as readonly unknown[]).includes(value);
}

function isNotificationKind(value: unknown): value is NotificationCreateParams["kind"] {
  return (NOTIFICATION_KINDS as readonly unknown[]).includes(value);
}

function isNotificationSeverity(value: unknown): value is NotificationSeverity {
  return (NOTIFICATION_SEVERITIES as readonly unknown[]).includes(value);
}

function isNotificationStatus(value: unknown): value is NotificationStatus {
  return (NOTIFICATION_STATUSES as readonly unknown[]).includes(value);
}

function isErrnoException(value: unknown): value is NodeJS.ErrnoException {
  return value instanceof Error && "code" in value;
}

function writeLine(socket: Socket, line: string): void {
  if (socket.destroyed) {
    return;
  }
  socket.write(Buffer.from(`${line}\n`));
}

function timestamp(): string {
  return new Date().toISOString();
}

function cloneValue<T>(value: T): T {
  return structuredClone(value);
}

function messageFromUnknown(source: unknown): string {
  if (source instanceof Error) {
    return source.message;
  }
  return String(source);
}
