import type {
  AgentActivity,
  NotificationRecord,
  ProtocolEvent,
  SessionInfo,
} from "@pohunek/protocol";

export interface ReducedSession {
  readonly session: SessionInfo;
  readonly attachStreamIds: readonly string[];
}

export interface HostDataState {
  readonly sessions: Readonly<Record<string, ReducedSession>>;
  readonly notifications: Readonly<Record<string, NotificationRecord>>;
}

export type ReducerEvent = ProtocolEvent | ({ readonly event: string } & Record<string, unknown>);

export function emptyHostDataState(): HostDataState {
  return { sessions: {}, notifications: {} };
}

export function hostDataFromSnapshot(
  sessions: readonly SessionInfo[],
  notifications: readonly NotificationRecord[],
): HostDataState {
  const sessionRecords: Record<string, ReducedSession> = {};
  for (const session of sessions) {
    sessionRecords[session.id] = {
      session: structuredClone(session),
      attachStreamIds: [],
    };
  }

  const notificationRecords: Record<string, NotificationRecord> = {};
  for (const notification of notifications) {
    if (notification.status !== "deleted") {
      notificationRecords[notification.id] = structuredClone(notification);
    }
  }
  return { sessions: sessionRecords, notifications: notificationRecords };
}

export function reduceHostEvent(state: HostDataState, event: ReducerEvent): HostDataState {
  if (isEventName(event, "session_created") || isEventName(event, "session_updated") || isEventName(event, "session_stopped")) {
    return upsertSession(state, event.session);
  }
  if (isEventName(event, "session_removed")) {
    return removeSession(state, event.session.id);
  }
  if (isEventName(event, "agent_state")) {
    return updateAgentState(state, event.session_id, event.activity, event.source);
  }
  if (isEventName(event, "attach_opened")) {
    return updateAttach(state, event.session_id, event.stream_id, true);
  }
  if (isEventName(event, "attach_closed")) {
    return updateAttach(state, event.session_id, event.stream_id, false);
  }
  if (isEventName(event, "notification_created") || isEventName(event, "notification_updated")) {
    return upsertNotification(state, event.record);
  }
  if (isEventName(event, "notification_deleted")) {
    return removeNotification(state, event.notification_id);
  }
  return state;
}

function upsertSession(state: HostDataState, session: SessionInfo): HostDataState {
  const existing = state.sessions[session.id];
  return {
    ...state,
    sessions: {
      ...state.sessions,
      [session.id]: {
        session: structuredClone(session),
        attachStreamIds: existing?.attachStreamIds ?? [],
      },
    },
  };
}

function removeSession(state: HostDataState, sessionId: string): HostDataState {
  if (state.sessions[sessionId] === undefined) {
    return state;
  }
  const sessions = { ...state.sessions };
  delete sessions[sessionId];
  return { ...state, sessions };
}

function updateAgentState(
  state: HostDataState,
  sessionId: string,
  activity: AgentActivity,
  source: SessionInfo["state_source"],
): HostDataState {
  const existing = state.sessions[sessionId];
  if (existing === undefined) {
    return state;
  }
  return {
    ...state,
    sessions: {
      ...state.sessions,
      [sessionId]: {
        ...existing,
        session: {
          ...existing.session,
          activity,
          state_source: source,
        },
      },
    },
  };
}

function updateAttach(
  state: HostDataState,
  sessionId: string,
  streamId: string,
  opened: boolean,
): HostDataState {
  const existing = state.sessions[sessionId];
  if (existing === undefined) {
    return state;
  }

  const prior = existing.attachStreamIds;
  const attachStreamIds = opened
    ? prior.includes(streamId) ? prior : [...prior, streamId]
    : prior.filter((candidate) => candidate !== streamId);
  if (attachStreamIds === prior) {
    return state;
  }
  return {
    ...state,
    sessions: {
      ...state.sessions,
      [sessionId]: { ...existing, attachStreamIds },
    },
  };
}

function upsertNotification(state: HostDataState, notification: NotificationRecord): HostDataState {
  if (notification.status === "deleted") {
    return removeNotification(state, notification.id);
  }
  return {
    ...state,
    notifications: {
      ...state.notifications,
      [notification.id]: structuredClone(notification),
    },
  };
}

function removeNotification(state: HostDataState, notificationId: string): HostDataState {
  if (state.notifications[notificationId] === undefined) {
    return state;
  }
  const notifications = { ...state.notifications };
  delete notifications[notificationId];
  return { ...state, notifications };
}

function isEventName<Name extends ProtocolEvent["event"]>(
  event: ReducerEvent,
  name: Name,
): event is Extract<ProtocolEvent, { readonly event: Name }> {
  return event.event === name;
}
