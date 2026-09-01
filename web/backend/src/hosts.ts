import type { DaemonHealthResult, HostRecord } from "@pohunek/protocol";
import { connectLocal } from "@pohunek/sdk";
import type { DaemonTarget } from "./relay";
import { externalFqdnSelector, externalPeerSelector } from "./identity";
import { errorClass, stdoutLogger, type BackendLogger } from "./log";

const LOCAL_HOST = "local";
const MILLISECONDS_PER_SECOND = 1_000;

export type HostReachability = HostRecord["classification"];

export interface BackendHostEntry {
  readonly host: string;
  readonly reachability: HostReachability;
  readonly daemon_version?: string;
  readonly protocol_version?: number;
}

export interface StartHostsPipelineOptions {
  readonly daemonSocketPath: string;
  readonly discoverIntervalSeconds: number;
  readonly logger?: BackendLogger;
}

export type BackendStartupErrorKind = "localDaemonUnavailable";

export class BackendStartupError extends Error {
  public override readonly name = "BackendStartupError";
  public readonly kind: BackendStartupErrorKind;
  public readonly socketPath: string;
  public override readonly cause: unknown;

  public constructor(socketPath: string, cause: unknown) {
    super(
      `Cannot start @pohunek/backend: the local daemon at ${socketPath} is unreachable or incompatible. `
        + `Start a compatible pohunekd instance, or set POHUNEK_BACKEND_DAEMON_SOCKET to its Unix socket.`,
    );
    this.kind = "localDaemonUnavailable";
    this.socketPath = socketPath;
    this.cause = cause;
  }
}

export interface HostsPipelineHandle {
  snapshot(): readonly BackendHostEntry[];
  resolveTargetForHost(host: string): Promise<DaemonTarget | undefined>;
  refresh(): Promise<void>;
  close(): Promise<void>;
}

interface DiscoverySnapshot {
  readonly health: DaemonHealthResult;
  readonly records: readonly HostRecord[];
}

class HostsPipeline implements HostsPipelineHandle {
  private readonly options: StartHostsPipelineOptions;
  private readonly logger: BackendLogger;
  private hosts: readonly BackendHostEntry[] = [];
  private targets: ReadonlyMap<string, DaemonTarget> = new Map();
  private timer: ReturnType<typeof setInterval> | undefined;
  private activeRefresh: Promise<void> | undefined;

  public constructor(options: StartHostsPipelineOptions) {
    this.options = options;
    this.logger = options.logger ?? stdoutLogger;
  }

  public async start(): Promise<void> {
    try {
      await this.refresh();
    } catch (error: unknown) {
      throw new BackendStartupError(this.options.daemonSocketPath, error);
    }

    const intervalMilliseconds = this.options.discoverIntervalSeconds * MILLISECONDS_PER_SECOND;
    this.timer = setInterval((): void => {
      void this.refresh().catch((error: unknown): void => {
        this.logger.log({
          level: "error",
          event: "host_discovery",
          method: "host.discover",
          host: LOCAL_HOST,
          status: "failed",
          error_class: errorClass(error),
        });
      });
    }, intervalMilliseconds);
  }

  public snapshot(): readonly BackendHostEntry[] {
    return this.hosts.map((entry) => ({ ...entry }));
  }

  public async resolveTargetForHost(host: string): Promise<DaemonTarget | undefined> {
    if (host === LOCAL_HOST) {
      return this.targets.get(host);
    }
    await this.activeRefresh;
    await this.refresh();
    return this.targets.get(host);
  }

  public refresh(): Promise<void> {
    if (this.activeRefresh !== undefined) {
      return this.activeRefresh;
    }

    const startedAt = performance.now();
    const refresh = this.discover()
      .then((snapshot): void => {
        this.applyDiscovery(snapshot);
        this.logger.log({
          level: "info",
          event: "host_discovery",
          method: "host.discover",
          host: LOCAL_HOST,
          duration_ms: elapsedMilliseconds(startedAt),
          status: "ok",
        });
      })
      .finally((): void => {
        this.activeRefresh = undefined;
      });
    this.activeRefresh = refresh;
    return refresh;
  }

  public async close(): Promise<void> {
    if (this.timer !== undefined) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
    await this.activeRefresh;
  }

  private async discover(): Promise<DiscoverySnapshot> {
    this.logger.log({
      level: "info",
      event: "daemon_connection",
      host: LOCAL_HOST,
      lifecycle: "connecting",
      status: "started",
    });
    const client = await connectLocal(this.options.daemonSocketPath);
    try {
      await client.handshake();
      const health = await client.call("daemon.health", null);
      const records = await client.call("host.discover", { force: true });
      this.logger.log({
        level: "info",
        event: "daemon_connection",
        host: LOCAL_HOST,
        lifecycle: "connected",
        status: "ok",
      });
      return { health, records };
    } finally {
      await client.close();
    }
  }

  private applyDiscovery(snapshot: DiscoverySnapshot): void {
    const nextHosts: BackendHostEntry[] = [localEntry(snapshot.health)];
    const nextTargets = new Map<string, DaemonTarget>([
      [LOCAL_HOST, { kind: "unix", socketPath: this.options.daemonSocketPath }],
    ]);
    const seen = new Set<string>([LOCAL_HOST]);

    for (const record of snapshot.records) {
      const host = hostIdentifier(record);
      if (host === undefined || seen.has(host)) {
        continue;
      }
      seen.add(host);

      if (record.classification === "reachable_daemon") {
        nextHosts.push({
          host,
          reachability: record.classification,
          daemon_version: record.daemon_version,
        });
        if (record.address !== null) {
          nextTargets.set(host, {
            kind: "tcp",
            host: record.address,
            port: record.port,
          });
        }
        continue;
      }

      if (record.classification === "version_mismatch") {
        nextHosts.push({
          host,
          reachability: record.classification,
          protocol_version: record.daemon_protocol_version,
        });
        continue;
      }

      nextHosts.push({ host, reachability: record.classification });
    }

    for (const previous of this.hosts) {
      if (previous.host !== LOCAL_HOST && !seen.has(previous.host)) {
        nextHosts.push({ host: previous.host, reachability: "unreachable" });
      }
    }

    this.hosts = nextHosts;
    this.targets = nextTargets;
  }
}

export async function startHostsPipeline(
  options: StartHostsPipelineOptions,
): Promise<HostsPipelineHandle> {
  const pipeline = new HostsPipeline(options);
  await pipeline.start();
  return pipeline;
}

function localEntry(health: DaemonHealthResult): BackendHostEntry {
  return {
    host: LOCAL_HOST,
    reachability: "reachable_daemon",
    daemon_version: health.daemon_version,
    protocol_version: health.protocol_version,
  };
}

function hostIdentifier(record: HostRecord): string | undefined {
  const identity = record.peer_id !== null && record.peer_id.length > 0
    ? externalPeerSelector(record.peer_id)
    : record.fqdn !== null && record.fqdn.length > 0
      ? externalFqdnSelector(record.fqdn)
      : undefined;
  return identity === undefined ? undefined : `${record.overlay}:${identity}`;
}

function elapsedMilliseconds(startedAt: number): number {
  return Math.round((performance.now() - startedAt) * 100) / 100;
}
