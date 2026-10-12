import type {
  AgentActivity,
  HostRecord,
  NotificationCreateParams,
  NotificationId,
  NotificationRecord,
  SessionId,
  SessionTurnEvent,
  SessionTurnEventKind,
  StateSource,
  SubagentInfo,
  TerminalDimensions,
} from "@pohunek/protocol";

export type ScenarioNotificationInput =
  & NotificationCreateParams
  & Partial<
    Pick<
      NotificationRecord,
      | "id"
      | "status"
      | "created_at"
      | "read_at"
      | "acked_at"
      | "archived_at"
      | "deleted_at"
      | "superseded_by"
    >
  >;

/**
 * A turn event a scenario can signal; `exited` follows a session's transition
 * into a terminal state instead.
 */
export type ScenarioTurnEventKind = Exclude<SessionTurnEventKind, "exited">;

/** A terminal size delivered to the fixture daemon over `session.resize`. */
export interface ScenarioResize {
  readonly cols: number;
  readonly rows: number;
}

export interface ScenarioBackend {
  setAgentState(sessionId: SessionId, activity: AgentActivity, source: StateSource): void;
  setSubagentState(sessionId: SessionId, subagent: SubagentInfo): void;
  removeSession(sessionId: SessionId): void;
  recordTurnEvent(
    sessionId: SessionId,
    kind: ScenarioTurnEventKind,
    notificationId?: NotificationId,
  ): SessionTurnEvent;
  createScenarioNotification(input: ScenarioNotificationInput): NotificationRecord;
  deleteNotification(id: NotificationId): void;
  initialAttachDimensions(sessionId: SessionId): ReadonlyArray<TerminalDimensions>;
  inputs(sessionId: SessionId): ReadonlyArray<Uint8Array>;
  resizes(sessionId: SessionId): ReadonlyArray<ScenarioResize>;
  replaceRuntime(sessionId: SessionId, workerInstanceId?: string): void;
  writeToPty(sessionId: SessionId, bytes: Uint8Array): number;
  queuePtyOutput(sessionId: SessionId, bytes: Uint8Array): void;
  setRetainedOutput(
    sessionId: SessionId,
    bytes: Uint8Array,
    historyStartOffset: number | bigint,
    workerInstanceId?: string,
  ): void;
  setDiscoveredHosts(hosts: readonly HostRecord[]): void;
  stopAbruptly(): Promise<void>;
}

const DEFAULT_AGENT_STATE_SOURCE = "report";

export class FixtureScenario {
  private readonly backend: ScenarioBackend;

  public constructor(backend: ScenarioBackend) {
    this.backend = backend;
  }

  public setAgentState(
    sessionId: SessionId,
    activity: AgentActivity,
    source: StateSource = DEFAULT_AGENT_STATE_SOURCE,
  ): void {
    this.backend.setAgentState(sessionId, activity, source);
  }

  public setSubagentState(sessionId: SessionId, subagent: SubagentInfo): void {
    this.backend.setSubagentState(sessionId, subagent);
  }

  /** Removes a fixture session and emits `session_removed` to subscribers. */
  public removeSession(sessionId: SessionId): void {
    this.backend.removeSession(sessionId);
  }

  /**
   * Records a turn event for `session.wait_turn` waiters and wakes them.
   *
   * It stands for a signal the daemon has already accepted and debounced (an
   * admitted provider hook report for a held session, or the projector's
   * blocked agent), so it is recorded at once and independently of the
   * notification inbox. The fixture applies no drop rule; record only signals
   * the daemon would keep. Pass `notificationId` to model an event whose inbox
   * record the policy kept.
   */
  public recordTurnEvent(
    sessionId: SessionId,
    kind: ScenarioTurnEventKind,
    notificationId?: NotificationId,
  ): SessionTurnEvent {
    return this.backend.recordTurnEvent(sessionId, kind, notificationId);
  }

  public createNotification(input: ScenarioNotificationInput): NotificationRecord {
    return this.backend.createScenarioNotification(input);
  }

  /** Removes a fixture notification and emits `notification_deleted` to subscribers. */
  public deleteNotification(id: NotificationId): void {
    this.backend.deleteNotification(id);
  }

  /** Returns the initial terminal geometry supplied with each session attach. */
  public initialAttachDimensions(sessionId: SessionId): ReadonlyArray<TerminalDimensions> {
    return this.backend.initialAttachDimensions(sessionId);
  }

  /** Returns a snapshot of terminal sizes delivered for one session. */
  public resizes(sessionId: SessionId): ReadonlyArray<ScenarioResize> {
    return this.backend.resizes(sessionId);
  }

  /** Returns input fragments delivered through `session.input`. */
  public inputs(sessionId: SessionId): ReadonlyArray<Uint8Array> {
    return this.backend.inputs(sessionId);
  }

  /** Replaces one live runtime and wakes runtime-scoped waiters. */
  public replaceRuntime(sessionId: SessionId, workerInstanceId?: string): void {
    this.backend.replaceRuntime(sessionId, workerInstanceId);
  }

  public writeToPty(sessionId: SessionId, bytes: Uint8Array): number {
    return this.backend.writeToPty(sessionId, bytes);
  }

  public queuePtyOutput(sessionId: SessionId, bytes: Uint8Array): void {
    this.backend.queuePtyOutput(sessionId, bytes);
  }

  /** Replaces retained output, optionally creating an explicit history gap. */
  public setRetainedOutput(
    sessionId: SessionId,
    bytes: Uint8Array,
    historyStartOffset: number | bigint = 0,
    workerInstanceId?: string,
  ): void {
    this.backend.setRetainedOutput(sessionId, bytes, historyStartOffset, workerInstanceId);
  }

  public setDiscoveredHosts(hosts: readonly HostRecord[]): void {
    this.backend.setDiscoveredHosts(hosts);
  }

  public stopAbruptly(): Promise<void> {
    return this.backend.stopAbruptly();
  }
}
