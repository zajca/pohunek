import type {
  Methods,
  NotificationRecord,
  NotificationUpdateParams,
  ProtocolEvent,
} from "@pohunek/protocol";
import { PROTOCOL_VERSION } from "@pohunek/protocol";
import { ClientError, type CatchAllEvent } from "@pohunek/sdk/browser";
import {
  WorkspaceActions,
  type ActionCaller,
  type NotificationRollback,
  type OptimisticNotificationCallbacks,
} from "./actions";
import { attachSession, type SessionAttachment } from "./attach";
import { HostConnection } from "./host-connection";
import {
  emptyHostDataState,
  hostDataFromSnapshot,
  reduceHostEvent,
  type HostDataState,
} from "./reducer";
import {
  MutableSnapshotStore,
  hostResourceKey,
  type HostedNotification,
  type HostedSession,
  type HostConnectionState,
  type HostDescriptor,
  type HostReachability,
  type HostsSnapshot,
  type NotificationsSnapshot,
  type SessionsSnapshot,
  type SnapshotStore,
} from "./stores";

/** Host discovery refreshes periodically so backend routing changes propagate without reload. */
export const HOST_LIST_REFRESH_INTERVAL_MS = 10_000;

const HOSTS_API_PATH = "/api/hosts";
const UNREACHABLE_HOST_REASON = "host discovery marked the daemon unreachable";
const CANDIDATE_HOST_REASON = "host discovery did not find a reachable daemon";
const UNKNOWN_VERSION_MISMATCH_REASON = "host discovery reported a protocol version mismatch";
const VALID_REACHABILITY = new Set<HostReachability>([
  "reachable_daemon",
  "version_mismatch",
  "unreachable",
  "candidate",
]);

export type HostsSource = () => Promise<readonly HostDescriptor[]>;

export interface WorkspaceOptions {
  readonly baseUrl: string;
  readonly hosts: HostsSource;
}

export interface Workspace {
  readonly hosts: SnapshotStore<HostsSnapshot>;
  readonly sessions: SnapshotStore<SessionsSnapshot>;
  readonly notifications: SnapshotStore<NotificationsSnapshot>;
  readonly actions: WorkspaceActions;
  attach(host: string, sessionId: string): Promise<SessionAttachment>;
  close(): Promise<void>;
}

export class PohunekWorkspace implements Workspace, ActionCaller, OptimisticNotificationCallbacks {
  public readonly hosts = new MutableSnapshotStore<HostsSnapshot>({});
  public readonly sessions = new MutableSnapshotStore<SessionsSnapshot>({});
  public readonly notifications = new MutableSnapshotStore<NotificationsSnapshot>({
    records: {},
    unreadCount: 0,
  });
  public readonly actions: WorkspaceActions;

  private readonly baseUrl: string;
  private readonly hostsSource: HostsSource;
  private readonly connections = new Map<string, HostConnection>();
  private readonly descriptors = new Map<string, HostDescriptor>();
  private readonly connectionStates = new Map<string, HostConnectionState>();
  private readonly hostData = new Map<string, HostDataState>();
  private readonly notificationVersions = new Map<string, number>();
  private readonly refreshTimer: ReturnType<typeof setInterval>;
  private refreshTask: Promise<void> | undefined;
  private closed = false;

  public constructor(options: WorkspaceOptions) {
    validateWorkspaceOptions(options);
    this.baseUrl = options.baseUrl;
    this.hostsSource = options.hosts;
    this.actions = new WorkspaceActions(this, this);
    void this.refreshHosts();
    this.refreshTimer = setInterval((): void => {
      void this.refreshHosts();
    }, HOST_LIST_REFRESH_INTERVAL_MS);
  }

  public call<K extends keyof Methods>(
    host: string,
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]> {
    const connection = this.connections.get(host);
    if (connection === undefined) {
      return Promise.reject(ClientError.remoteDaemonUnavailable(host));
    }
    return connection.call(method, params);
  }

  public attach(host: string, sessionId: string): Promise<SessionAttachment> {
    return attachSession(this.baseUrl, host, sessionId, this);
  }

  public begin(host: string, params: NotificationUpdateParams): NotificationRollback | undefined {
    const state = this.hostData.get(host);
    const previous = state?.notifications[params.id];
    if (state === undefined || previous === undefined) {
      return undefined;
    }

    const version = this.bumpNotificationVersion(host, params.id);
    const notification = { ...previous, status: params.status };
    this.hostData.set(host, {
      ...state,
      notifications: { ...state.notifications, [params.id]: notification },
    });
    this.publishDataStores();
    return { host, id: params.id, version, previous: structuredClone(previous) };
  }

  public commit(host: string, record: NotificationRecord): void {
    this.replaceNotification(host, record);
  }

  public rollback(change: NotificationRollback | undefined): void {
    if (change === undefined) {
      return;
    }
    const key = hostResourceKey(change.host, change.id);
    if (this.notificationVersions.get(key) !== change.version) {
      return;
    }
    this.replaceNotification(change.host, change.previous);
  }

  public async close(): Promise<void> {
    if (this.closed) {
      await this.refreshTask;
      return;
    }
    this.closed = true;
    clearInterval(this.refreshTimer);
    await this.refreshTask;
    const connections = Array.from(this.connections.values());
    this.connections.clear();
    await Promise.all(connections.map((connection) => connection.close()));
  }

  private refreshHosts(): Promise<void> {
    if (this.closed) {
      return Promise.resolve();
    }
    if (this.refreshTask !== undefined) {
      return this.refreshTask;
    }

    const task = this.hostsSource()
      .then((hosts): void => {
        if (!this.closed) {
          this.applyHostList(hosts);
        }
      })
      .catch((): void => {
        // A discovery failure leaves the last usable per-host partial results intact.
      })
      .finally((): void => {
        this.refreshTask = undefined;
      });
    this.refreshTask = task;
    return task;
  }

  private applyHostList(hosts: readonly HostDescriptor[]): void {
    const nextDescriptors = validatedHostList(hosts);

    for (const [host, connection] of this.connections) {
      const descriptor = nextDescriptors.get(host);
      if (descriptor === undefined || staticConnectionState(descriptor) !== undefined) {
        this.connections.delete(host);
        void connection.close();
      }
    }

    for (const host of this.descriptors.keys()) {
      if (!nextDescriptors.has(host)) {
        this.descriptors.delete(host);
        this.connectionStates.delete(host);
        this.hostData.delete(host);
        this.dropNotificationVersions(host);
      }
    }

    for (const [host, descriptor] of nextDescriptors) {
      this.descriptors.set(host, descriptor);
      const staticState = staticConnectionState(descriptor);
      if (staticState !== undefined) {
        this.connectionStates.set(host, staticState);
        if (!this.hostData.has(host)) {
          this.hostData.set(host, emptyHostDataState());
        }
        continue;
      }
      if (this.connections.has(host)) {
        continue;
      }

      this.connectionStates.set(host, { kind: "connecting" });
      if (!this.hostData.has(host)) {
        this.hostData.set(host, emptyHostDataState());
      }
      const connection = new HostConnection(this.baseUrl, host, {
        onState: (state): void => {
          if (this.connections.get(host) === connection) {
            this.connectionStates.set(host, state);
            this.publishHostsStore();
          }
        },
        onSnapshot: (sessions, notifications): void => {
          if (this.connections.get(host) === connection) {
            this.applyHostSnapshot(host, sessions, notifications);
          }
        },
        onEvent: (event): void => {
          if (this.connections.get(host) === connection) {
            this.applyHostEvent(host, event);
          }
        },
      });
      this.connections.set(host, connection);
      connection.start();
    }

    this.publishHostsStore();
    this.publishDataStores();
  }

  private applyHostSnapshot(
    host: string,
    sessions: Parameters<typeof hostDataFromSnapshot>[0],
    notifications: Parameters<typeof hostDataFromSnapshot>[1],
  ): void {
    const previous = this.hostData.get(host);
    this.hostData.set(host, hostDataFromSnapshot(sessions, notifications));
    const ids = new Set([
      ...Object.keys(previous?.notifications ?? {}),
      ...notifications.map((notification) => notification.id),
    ]);
    for (const id of ids) {
      this.bumpNotificationVersion(host, id);
    }
    this.publishDataStores();
  }

  private applyHostEvent(host: string, event: ProtocolEvent | CatchAllEvent): void {
    const state = this.hostData.get(host);
    if (state === undefined) {
      return;
    }
    const next = reduceHostEvent(state, event);
    if (next === state) {
      return;
    }
    this.hostData.set(host, next);
    const notificationId = notificationIdFromEvent(event);
    if (notificationId !== undefined) {
      this.bumpNotificationVersion(host, notificationId);
    }
    this.publishDataStores();
  }

  private replaceNotification(host: string, record: NotificationRecord): void {
    const state = this.hostData.get(host);
    if (state === undefined) {
      return;
    }
    const event: ProtocolEvent = {
      v: PROTOCOL_VERSION,
      event: "notification_updated",
      record,
    };
    const next = reduceHostEvent(state, event);
    this.hostData.set(host, next);
    this.bumpNotificationVersion(host, record.id);
    this.publishDataStores();
  }

  private bumpNotificationVersion(host: string, id: string): number {
    const key = hostResourceKey(host, id);
    const next = (this.notificationVersions.get(key) ?? 0) + 1;
    this.notificationVersions.set(key, next);
    return next;
  }

  private dropNotificationVersions(host: string): void {
    for (const key of this.notificationVersions.keys()) {
      if (key.startsWith(`["${escapeJsonKey(host)}",`)) {
        this.notificationVersions.delete(key);
      }
    }
  }

  private publishHostsStore(): void {
    const snapshot: Record<string, HostsSnapshot[string]> = {};
    for (const [host, descriptor] of this.descriptors) {
      snapshot[host] = {
        ...descriptor,
        connection: this.connectionStates.get(host) ?? { kind: "connecting" },
      };
    }
    this.hosts.replace(snapshot);
  }

  private publishDataStores(): void {
    const sessions: Record<string, HostedSession> = {};
    const notifications: Record<string, HostedNotification> = {};
    let unreadCount = 0;

    for (const [host, state] of this.hostData) {
      for (const [id, reduced] of Object.entries(state.sessions)) {
        sessions[hostResourceKey(host, id)] = {
          host,
          session: reduced.session,
          attachStreamIds: reduced.attachStreamIds,
        };
      }
      for (const [id, notification] of Object.entries(state.notifications)) {
        notifications[hostResourceKey(host, id)] = { host, notification };
        if (notification.status === "unread") {
          unreadCount += 1;
        }
      }
    }

    this.sessions.replace(sessions);
    this.notifications.replace({ records: notifications, unreadCount });
  }
}

function staticConnectionState(descriptor: HostDescriptor): HostConnectionState | undefined {
  switch (descriptor.reachability) {
    case "reachable_daemon":
      return undefined;
    case "version_mismatch":
      return descriptor.protocol_version === undefined
        ? { kind: "error", reason: UNKNOWN_VERSION_MISMATCH_REASON }
        : { kind: "version_mismatch", theirs: descriptor.protocol_version };
    case "unreachable":
      return { kind: "error", reason: UNREACHABLE_HOST_REASON };
    case "candidate":
      return { kind: "error", reason: CANDIDATE_HOST_REASON };
  }
}

export function createWorkspace(options: WorkspaceOptions): Workspace {
  return new PohunekWorkspace(options);
}

export default function createBackendHostsSource(baseUrl: string): HostsSource {
  if (baseUrl.trim().length === 0) {
    throw new Error("baseUrl is required");
  }
  const url = new URL(HOSTS_API_PATH, baseUrl);
  return async (): Promise<readonly HostDescriptor[]> => {
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`GET ${url.pathname} failed with status ${response.status}`);
    }
    return parseHostsResponse(await response.json());
  };
}

function validateWorkspaceOptions(options: WorkspaceOptions): void {
  if (options.baseUrl.trim().length === 0) {
    throw new Error("baseUrl is required");
  }
  if (typeof options.hosts !== "function") {
    throw new Error("hosts source is required");
  }
}

function validatedHostList(hosts: readonly HostDescriptor[]): Map<string, HostDescriptor> {
  const result = new Map<string, HostDescriptor>();
  for (const descriptor of hosts) {
    if (descriptor.host.trim().length === 0 || !VALID_REACHABILITY.has(descriptor.reachability)) {
      throw new Error("host source returned an invalid host descriptor");
    }
    if (result.has(descriptor.host)) {
      throw new Error(`host source returned duplicate host '${descriptor.host}'`);
    }
    result.set(descriptor.host, structuredClone(descriptor));
  }
  return result;
}

function parseHostsResponse(value: unknown): readonly HostDescriptor[] {
  if (!Array.isArray(value)) {
    throw new Error("GET /api/hosts returned a non-array response");
  }
  return value.map((entry): HostDescriptor => {
    if (!isRecord(entry)) {
      throw new Error("GET /api/hosts returned an invalid host entry");
    }
    const host = entry["host"];
    const reachability = entry["reachability"];
    const daemonVersion = entry["daemon_version"];
    const protocolVersion = entry["protocol_version"];
    if (
      typeof host !== "string"
      || typeof reachability !== "string"
      || !VALID_REACHABILITY.has(reachability as HostReachability)
      || (daemonVersion !== undefined && typeof daemonVersion !== "string")
      || (protocolVersion !== undefined && typeof protocolVersion !== "number")
    ) {
      throw new Error("GET /api/hosts returned an invalid host entry");
    }
    return optionalHostDescriptor(
      host,
      reachability as HostReachability,
      daemonVersion,
      protocolVersion,
    );
  });
}

function optionalHostDescriptor(
  host: string,
  reachability: HostReachability,
  daemonVersion: string | undefined,
  protocolVersion: number | undefined,
): HostDescriptor {
  return {
    host,
    reachability,
    ...(daemonVersion === undefined ? {} : { daemon_version: daemonVersion }),
    ...(protocolVersion === undefined ? {} : { protocol_version: protocolVersion }),
  };
}

function notificationIdFromEvent(event: ProtocolEvent | CatchAllEvent): string | undefined {
  if (event.event === "notification_deleted" && "notification_id" in event) {
    return typeof event.notification_id === "string" ? event.notification_id : undefined;
  }
  if (
    (event.event === "notification_created" || event.event === "notification_updated")
    && "record" in event
    && isRecord(event.record)
  ) {
    return typeof event.record["id"] === "string" ? event.record["id"] : undefined;
  }
  return undefined;
}

function escapeJsonKey(value: string): string {
  return JSON.stringify(value).slice(1, -1);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
