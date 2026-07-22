import type {
  HostsSnapshot,
  NotificationsSnapshot,
  SessionsSnapshot,
  SnapshotStore,
  Workspace,
} from "@pohunek/client-core";
import type { Readable, Unsubscriber } from "svelte/store";

/** Adapts a client-core snapshot store to Svelte's readable store contract. */
export function snapshotReadable<T>(source: SnapshotStore<T>): Readable<T> {
  return {
    subscribe(run): Unsubscriber {
      run(source.snapshot());
      return source.subscribe(run);
    },
  };
}

export interface WorkspaceStores {
  readonly hosts: Readable<HostsSnapshot>;
  readonly sessions: Readable<SessionsSnapshot>;
  readonly notifications: Readable<NotificationsSnapshot>;
}

/** Creates the three Svelte-facing stores exposed by one workspace. */
export function workspaceStores(workspace: Workspace): WorkspaceStores {
  return {
    hosts: snapshotReadable(workspace.hosts),
    sessions: snapshotReadable(workspace.sessions),
    notifications: snapshotReadable(workspace.notifications),
  };
}
