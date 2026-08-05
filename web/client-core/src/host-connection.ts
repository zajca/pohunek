import {
  Client,
  ClientError,
  nextRequestId,
  type CatchAllEvent,
  type Subscription,
} from "@pohunek/sdk/browser";
import {
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type Methods,
  type NotificationRecord,
  type ProtocolEvent,
  type ProtocolVersion,
  type SessionInfo,
} from "@pohunek/protocol";
import type { HostConnectionState } from "./stores";

/** Initial retry delay keeps transient relay failures responsive without spinning. */
export const INITIAL_RECONNECT_DELAY_MS = 250;
/** Retry delay is capped so a recovered host is eventually noticed. */
export const MAX_RECONNECT_DELAY_MS = 5_000;
/** Notification snapshots are fetched in bounded pages and followed through every cursor. */
export const NOTIFICATION_PAGE_SIZE = 100;

export interface HostConnectionCallbacks {
  readonly onState: (state: HostConnectionState) => void;
  readonly onSnapshot: (
    sessions: readonly SessionInfo[],
    notifications: readonly NotificationRecord[],
  ) => void;
  readonly onEvent: (event: ProtocolEvent | CatchAllEvent) => void;
}

interface ActiveRequestClient {
  readonly client: Client;
  readonly generation: number;
}

class VersionMismatchSignal extends Error {
  public readonly theirs: ProtocolVersion;

  public constructor(theirs: ProtocolVersion) {
    super(`daemon protocol version ${theirs} does not match client protocol version ${PROTOCOL_VERSION}`);
    this.theirs = theirs;
  }
}

export class HostConnection {
  private readonly baseUrl: string;
  private readonly host: string;
  private readonly callbacks: HostConnectionCallbacks;
  private activeRequest: ActiveRequestClient | undefined;
  private activeSubscriptionClient: Client | undefined;
  private operationQueue: Promise<void> = Promise.resolve();
  private runTask: Promise<void> | undefined;
  private retryTimer: ReturnType<typeof setTimeout> | undefined;
  private wakeRetry: (() => void) | undefined;
  private generation = 0;
  private closed = false;

  public constructor(baseUrl: string, host: string, callbacks: HostConnectionCallbacks) {
    this.baseUrl = baseUrl;
    this.host = host;
    this.callbacks = callbacks;
  }

  public start(): void {
    if (this.runTask !== undefined) {
      return;
    }
    this.runTask = this.run();
  }

  public async call<K extends keyof Methods>(
    method: K,
    params: Methods[K]["params"],
  ): Promise<Methods[K]["output"]> {
    const active = this.activeRequest;
    if (active === undefined || this.closed) {
      throw ClientError.remoteDaemonUnavailable(this.host);
    }

    const operation = this.operationQueue.then(async (): Promise<Methods[K]["output"]> => {
      if (this.activeRequest !== active || this.closed) {
        throw ClientError.remoteDaemonUnavailable(this.host);
      }
      return active.client.call(method, params);
    });
    this.operationQueue = operation.then(
      (): void => undefined,
      (): void => undefined,
    );

    try {
      return await operation;
    } catch (error: unknown) {
      if (this.activeRequest === active && invalidatesConnection(error)) {
        this.invalidateActiveConnection();
      }
      throw error;
    }
  }

  public sessionOutput(
    params: Methods["session.output"]["params"],
  ): Promise<Methods["session.output"]["output"]> {
    if (params.wait_ms === undefined) {
      return this.call("session.output", params);
    }
    return this.requireActiveClient().client.sessionOutput(params);
  }

  public sessionWait(
    params: Methods["session.wait"]["params"],
  ): Promise<Methods["session.wait"]["output"]> {
    return this.requireActiveClient().client.sessionWait(params);
  }

  public async close(): Promise<void> {
    if (this.closed) {
      await this.runTask;
      return;
    }
    this.closed = true;
    this.cancelRetryWait();
    this.invalidateActiveConnection();
    await this.runTask;
  }

  private requireActiveClient(): ActiveRequestClient {
    const active = this.activeRequest;
    if (active === undefined || this.closed) {
      throw ClientError.remoteDaemonUnavailable(this.host);
    }
    return active;
  }

  private async run(): Promise<void> {
    let retryDelay = INITIAL_RECONNECT_DELAY_MS;
    while (!this.closed) {
      this.callbacks.onState({ kind: "connecting" });
      const generation = this.nextGeneration();
      let requestClient: Client | undefined;
      let subscriptionClient: Client | undefined;

      try {
        requestClient = await Client.connectWs(this.baseUrl, this.host);
        const health = await requestClient.call("daemon.health", null);
        assertCompatibleVersion(health.protocol_version);

        subscriptionClient = await Client.connectWs(this.baseUrl, this.host);
        const subscriptionHealth = await subscriptionClient.call("daemon.health", null);
        assertCompatibleVersion(subscriptionHealth.protocol_version);
        const subscription = await subscriptionClient.subscribe({
          v: SUPPORTED_PROTOCOL_VERSIONS,
          id: nextRequestId("subscribe"),
          method: "subscribe",
          params: null,
        });

        const sessions = await requestClient.call("session.list", {});
        const notifications = await fetchAllNotifications(requestClient);
        if (this.closed) {
          break;
        }

        this.activeRequest = { client: requestClient, generation };
        this.activeSubscriptionClient = subscriptionClient;
        this.callbacks.onSnapshot(sessions, notifications);
        this.callbacks.onState({ kind: "connected" });
        retryDelay = INITIAL_RECONNECT_DELAY_MS;
        await this.readEvents(subscription);
        throw ClientError.remoteDaemonUnavailable(this.host);
      } catch (error: unknown) {
        if (!this.closed) {
          this.callbacks.onState(connectionErrorState(error));
        }
      } finally {
        if (this.activeRequest?.generation === generation) {
          this.activeRequest = undefined;
        }
        if (this.activeSubscriptionClient === subscriptionClient) {
          this.activeSubscriptionClient = undefined;
        }
        await closeClients(requestClient, subscriptionClient);
      }

      if (!this.closed) {
        await this.waitForRetry(retryDelay);
        retryDelay = Math.min(retryDelay * 2, MAX_RECONNECT_DELAY_MS);
      }
    }
  }

  private async readEvents(subscription: Subscription): Promise<void> {
    while (!this.closed) {
      const event = await subscription.nextEvent();
      if (event === null) {
        return;
      }
      this.callbacks.onEvent(event);
    }
  }

  private nextGeneration(): number {
    this.generation += 1;
    return this.generation;
  }

  private invalidateActiveConnection(): void {
    const requestClient = this.activeRequest?.client;
    const subscriptionClient = this.activeSubscriptionClient;
    this.activeRequest = undefined;
    this.activeSubscriptionClient = undefined;
    void closeClients(requestClient, subscriptionClient);
  }

  private waitForRetry(delayMs: number): Promise<void> {
    return new Promise((resolve) => {
      this.wakeRetry = resolve;
      this.retryTimer = setTimeout((): void => {
        this.retryTimer = undefined;
        this.wakeRetry = undefined;
        resolve();
      }, delayMs);
    });
  }

  private cancelRetryWait(): void {
    if (this.retryTimer !== undefined) {
      clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
    }
    const wake = this.wakeRetry;
    this.wakeRetry = undefined;
    wake?.();
  }
}

async function fetchAllNotifications(client: Client): Promise<NotificationRecord[]> {
  const notifications: NotificationRecord[] = [];
  const seenCursors = new Set<string>();
  let cursor: string | undefined;
  do {
    const result = await client.call(
      "notification.list",
      cursor === undefined
        ? { limit: NOTIFICATION_PAGE_SIZE }
        : { limit: NOTIFICATION_PAGE_SIZE, cursor },
    );
    notifications.push(...result.notifications);
    cursor = result.next_cursor;
    if (cursor !== undefined && seenCursors.has(cursor)) {
      throw ClientError.framing(`notification.list repeated cursor '${cursor}'`);
    }
    if (cursor !== undefined) {
      seenCursors.add(cursor);
    }
  } while (cursor !== undefined);
  return notifications;
}

function assertCompatibleVersion(theirs: ProtocolVersion): void {
  if (theirs !== PROTOCOL_VERSION) {
    throw new VersionMismatchSignal(theirs);
  }
}

function connectionErrorState(error: unknown): HostConnectionState {
  if (error instanceof VersionMismatchSignal) {
    return { kind: "version_mismatch", theirs: error.theirs };
  }
  return {
    kind: "error",
    reason: error instanceof Error ? error.message : String(error),
  };
}

function invalidatesConnection(error: unknown): boolean {
  return !(error instanceof ClientError)
    || (error.kind !== "protocol" && error.kind !== "remoteProtocol");
}

async function closeClients(...clients: readonly (Client | undefined)[]): Promise<void> {
  const unique = new Set(clients.filter((client): client is Client => client !== undefined));
  await Promise.all(Array.from(unique, async (client): Promise<void> => {
    try {
      await client.close();
    } catch {
      // Closing is best-effort while replacing a failed connection.
    }
  }));
}
