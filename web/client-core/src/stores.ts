import type {
  NotificationRecord,
  ProtocolVersion,
  SessionInfo,
} from "@pohunek/protocol";
import type { RuntimeContinuity } from "./reducer";

export type HostReachability =
  | "reachable_daemon"
  | "version_mismatch"
  | "unreachable"
  | "candidate";

export interface HostDescriptor {
  readonly host: string;
  readonly reachability: HostReachability;
  readonly daemon_version?: string;
  readonly protocol_version?: ProtocolVersion;
}

export type HostConnectionState =
  | { readonly kind: "connecting" }
  | { readonly kind: "connected" }
  | { readonly kind: "error"; readonly reason: string }
  | { readonly kind: "version_mismatch"; readonly theirs: ProtocolVersion };

export interface HostSnapshot extends HostDescriptor {
  readonly connection: HostConnectionState;
}

export interface HostedSession {
  readonly host: string;
  readonly session: SessionInfo;
  readonly attachStreamIds: readonly string[];
  readonly runtimeContinuity: RuntimeContinuity;
}

export interface HostedNotification {
  readonly host: string;
  readonly notification: NotificationRecord;
}

export type HostsSnapshot = Readonly<Record<string, HostSnapshot>>;
export type SessionsSnapshot = Readonly<Record<string, HostedSession>>;

export interface NotificationsSnapshot {
  readonly records: Readonly<Record<string, HostedNotification>>;
  readonly unreadCount: number;
}

export type StoreListener<T> = (snapshot: T) => void;

export interface SnapshotStore<T> {
  snapshot(): T;
  subscribe(listener: StoreListener<T>): () => void;
}

export class MutableSnapshotStore<T> implements SnapshotStore<T> {
  private value: T;
  private readonly listeners = new Set<StoreListener<T>>();

  public constructor(initialValue: T) {
    this.value = immutableClone(initialValue);
  }

  public snapshot(): T {
    return this.value;
  }

  public subscribe(listener: StoreListener<T>): () => void {
    this.listeners.add(listener);
    return (): void => {
      this.listeners.delete(listener);
    };
  }

  public replace(value: T): void {
    this.value = immutableClone(value);
    for (const listener of this.listeners) {
      listener(this.value);
    }
  }
}

export function hostResourceKey(host: string, id: string): string {
  return JSON.stringify([host, id]);
}

function immutableClone<T>(value: T): T {
  return deepFreeze(structuredClone(value));
}

function deepFreeze<T>(value: T): T {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) {
    return value;
  }

  for (const child of Object.values(value)) {
    deepFreeze(child);
  }
  return Object.freeze(value);
}
