import { describe, expect, test } from "bun:test";
import { startRelay, type DaemonTarget, type RelayHandle } from "@pohunek/backend";
import {
  PROTOCOL_VERSION,
  type NotificationRecord,
  type SessionInfo,
} from "@pohunek/protocol";
import { ClientError, type RawStream } from "@pohunek/sdk";
import {
  DEFAULT_PTY_READY_BYTES,
  startFixtureDaemon,
  type FixtureDaemonHandle,
} from "@pohunek/testkit";
import createBackendHostsSource, {
  createWorkspace,
  hostDataFromSnapshot,
  hostResourceKey,
  reduceHostEvent,
  type HostDescriptor,
  type Workspace,
} from "@pohunek/client-core";

const LOOPBACK_HOST = "127.0.0.1";
const TEST_TIMEOUT_MS = 10_000;
const TEST_POLL_INTERVAL_MS = 10;
const TEST_COLS = 80;
const TEST_ROWS = 24;
const RESIZED_COLS = 132;
const RESIZED_ROWS = 43;
const PAGINATED_NOTIFICATION_COUNT = 101;
const DISCOVERED_HOST_COUNT = 97;
const NON_UTF8_PAYLOAD = Uint8Array.of(0x00, 0xff, 0x80, 0x61, 0xc3, 0x28);

describe("@pohunek/client-core", () => {
  test("fans out independently and marks down and version-mismatched hosts", async () => {
    const healthy = await startTcpFixture({ initialSessions: [session("s-healthy")] });
    const mismatch = await startTcpFixture({ protocolVersion: PROTOCOL_VERSION + 1 });
    const unavailable = await startTcpFixture({});
    const unavailableTarget = tcpTarget(unavailable);
    await unavailable.close();
    const relay = await relayFor(new Map([
      ["healthy", tcpTarget(healthy)],
      ["mismatch", tcpTarget(mismatch)],
      ["down", unavailableTarget],
    ]));
    const workspace = workspaceFor(relay, [host("healthy"), host("mismatch"), host("down")]);

    try {
      await waitFor(() => connectionKind(workspace, "healthy") === "connected");
      await waitFor(() => connectionKind(workspace, "down") === "error");
      await waitFor(() => connectionKind(workspace, "mismatch") === "version_mismatch");

      expect(connectionKind(workspace, "healthy")).toBe("connected");
      expect(workspace.hosts.snapshot()["mismatch"]?.connection).toEqual({
        kind: "version_mismatch",
        theirs: PROTOCOL_VERSION + 1,
      });
      expect(workspace.sessions.snapshot()[hostResourceKey("healthy", "s-healthy")]?.session.id)
        .toBe("s-healthy");
    } finally {
      await workspace.close();
      await relay.close();
      await healthy.close();
      await mismatch.close();
    }
  });

  test("connects only discovered daemon targets while preserving unreachable host markers", async () => {
    const local = await startTcpFixture({ initialSessions: [session("s-local-fast")] });
    const attemptedHosts: string[] = [];
    const relay = await startRelay({
      bindHost: LOOPBACK_HOST,
      port: 0,
      targets: (hostName): DaemonTarget | undefined => {
        attemptedHosts.push(hostName);
        return hostName === "local" ? tcpTarget(local) : undefined;
      },
      allowLoopbackBind: true,
    });
    const unreachableHosts = Array.from(
      { length: DISCOVERED_HOST_COUNT - 3 },
      (_, index): HostDescriptor => ({
        host: `offline-${index}`,
        reachability: "unreachable",
      }),
    );
    const workspace = workspaceFor(relay, [
      host("local"),
      ...unreachableHosts,
      { host: "candidate", reachability: "candidate" },
      {
        host: "mismatch",
        reachability: "version_mismatch",
        protocol_version: PROTOCOL_VERSION + 1,
      },
    ]);

    try {
      await waitFor(() => connectionKind(workspace, "local") === "connected");

      expect(Object.keys(workspace.hosts.snapshot()).length).toBe(DISCOVERED_HOST_COUNT);
      expect(workspace.hosts.snapshot()["offline-0"]).toEqual({
        host: "offline-0",
        reachability: "unreachable",
        connection: {
          kind: "error",
          reason: "host discovery marked the daemon unreachable",
        },
      });
      expect(workspace.hosts.snapshot()["candidate"]?.connection).toEqual({
        kind: "error",
        reason: "host discovery did not find a reachable daemon",
      });
      expect(workspace.hosts.snapshot()["mismatch"]?.connection).toEqual({
        kind: "version_mismatch",
        theirs: PROTOCOL_VERSION + 1,
      });
      expect(attemptedHosts.every((hostName) => hostName === "local")).toBe(true);
      expect(workspace.sessions.snapshot()[hostResourceKey("local", "s-local-fast")]?.session.id)
        .toBe("s-local-fast");
    } finally {
      await workspace.close();
      await relay.close();
      await local.close();
    }
  });

  test("reconnect replaces stale host data before returning to connected", async () => {
    let daemon = await startTcpFixture({
      initialSessions: [session("s-stale")],
      initialNotifications: [notification("n-stale", "unread")],
    });
    const address = requireTcpAddress(daemon);
    const relay = await relayFor(new Map([
      ["reconnect", { kind: "tcp", host: address.host, port: address.port }],
    ]));
    const workspace = workspaceFor(relay, [host("reconnect")]);
    const connectedSnapshots: string[][] = [];
    const unsubscribe = workspace.hosts.subscribe((snapshot): void => {
      if (snapshot["reconnect"]?.connection.kind === "connected") {
        connectedSnapshots.push(sessionIds(workspace, "reconnect"));
      }
    });

    try {
      await waitFor(() => connectionKind(workspace, "reconnect") === "connected");
      expect(sessionIds(workspace, "reconnect")).toEqual(["s-stale"]);

      await daemon.stopAbruptly();
      await waitFor(() => connectionKind(workspace, "reconnect") === "error");
      daemon = await startTcpFixture({
        port: address.port,
        initialSessions: [session("s-fresh")],
        initialNotifications: [notification("n-fresh", "unread")],
      });

      await waitFor(() => connectedSnapshots.length >= 2);
      expect(connectedSnapshots.at(-1)).toEqual(["s-fresh"]);
      expect(workspace.sessions.snapshot()[hostResourceKey("reconnect", "s-stale")]).toBeUndefined();
      expect(workspace.notifications.snapshot().records[hostResourceKey("reconnect", "n-stale")])
        .toBeUndefined();
      expect(workspace.notifications.snapshot().records[hostResourceKey("reconnect", "n-fresh")]?.notification.id)
        .toBe("n-fresh");
    } finally {
      unsubscribe();
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("reconnect keeps the same runtime generation and marks a changed generation as recovery", async () => {
    const live = session("s-runtime", "runtime-1");
    let daemon = await startTcpFixture({ initialSessions: [live] });
    const address = requireTcpAddress(daemon);
    const relay = await relayFor(new Map([
      ["runtime", { kind: "tcp", host: address.host, port: address.port }],
    ]));
    const workspace = workspaceFor(relay, [host("runtime")]);
    const key = hostResourceKey("runtime", live.id);

    try {
      await waitFor(() => connectionKind(workspace, "runtime") === "connected");
      expect(workspace.sessions.snapshot()[key]?.runtimeContinuity).toBe("initial");

      await daemon.stopAbruptly();
      await waitFor(() => connectionKind(workspace, "runtime") === "error");
      daemon = await startTcpFixture({
        port: address.port,
        initialSessions: [session("s-runtime", "runtime-1")],
      });

      await waitFor(() => workspace.sessions.snapshot()[key]?.runtimeContinuity === "reconnected");
      expect(workspace.sessions.snapshot()[key]?.session.runtime?.runtime_id).toBe("runtime-1");

      const recovered = hostDataFromSnapshot(
        [session("s-runtime", "runtime-2")],
        [],
        hostDataFromSnapshot([session("s-runtime", "runtime-1")], []),
      );
      expect(recovered.sessions["s-runtime"]?.runtimeContinuity).toBe("recovered");
      expect(recovered.sessions["s-runtime"]?.session.runtime?.runtime_id).toBe("runtime-2");
    } finally {
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("reduces runtime lifecycle events without losing the logical session", () => {
    const initial = hostDataFromSnapshot([session("s-runtime", "runtime-1")], []);
    const lost = reduceHostEvent(initial, {
      v: PROTOCOL_VERSION,
      event: "session_runtime_lost",
      session: {
        ...session("s-runtime", "runtime-1"),
        runtime: {
          ...session("s-runtime", "runtime-1").runtime!,
          state: "lost",
          loss_reason: "worker_missing",
        },
      },
    });
    expect(lost.sessions["s-runtime"]?.session.runtime?.state).toBe("lost");

    const recovered = reduceHostEvent(lost, {
      v: PROTOCOL_VERSION,
      event: "session_native_recovered",
      session: session("s-runtime", "runtime-2"),
    });
    expect(recovered.sessions["s-runtime"]?.runtimeContinuity).toBe("recovered");
    expect(recovered.sessions["s-runtime"]?.session.runtime?.runtime_id).toBe("runtime-2");
  });

  test("reduces every live session, attach, agent, and notification transition", async () => {
    const daemon = await startTcpFixture({});
    const relay = await relayFor(new Map([["events", tcpTarget(daemon)]]));
    const workspace = workspaceFor(relay, [host("events")]);

    try {
      await waitFor(() => connectionKind(workspace, "events") === "connected");
      const created = await workspace.actions.sessionNew("events", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const key = hostResourceKey("events", created.id);
      await waitFor(() => workspace.sessions.snapshot()[key] !== undefined);

      await workspace.actions.sessionResize("events", {
        session_id: created.id,
        cols: RESIZED_COLS,
        rows: RESIZED_ROWS,
      });
      await waitFor(() => workspace.sessions.snapshot()[key]?.session.cols === RESIZED_COLS);

      daemon.scenario.setAgentState(created.id, "blocked", "report");
      await waitFor(() => workspace.sessions.snapshot()[key]?.session.activity === "blocked");
      expect(workspace.sessions.snapshot()[key]?.session.state_source).toBe("report");

      const attachment = await workspace.attach("events", created.id);
      await waitFor(() => workspace.sessions.snapshot()[key]?.attachStreamIds.includes(attachment.streamId) === true);
      await attachment.detach();
      await waitFor(() => workspace.sessions.snapshot()[key]?.attachStreamIds.length === 0);

      await workspace.actions.sessionStop("events", created.id);
      await waitFor(() => workspace.sessions.snapshot()[key]?.session.state === "stopped");
      daemon.scenario.removeSession(created.id);
      await waitFor(() => workspace.sessions.snapshot()[key] === undefined);

      const notification = daemon.scenario.createNotification(notificationInput("n-events"));
      const notificationKey = hostResourceKey("events", notification.id);
      await waitFor(() => workspace.notifications.snapshot().records[notificationKey] !== undefined);
      expect(workspace.notifications.snapshot().unreadCount).toBe(1);

      await workspace.actions.notificationUpdate("events", { id: notification.id, status: "read" });
      await waitFor(() => workspace.notifications.snapshot().records[notificationKey]?.notification.status === "read");
      expect(workspace.notifications.snapshot().unreadCount).toBe(0);

      daemon.scenario.deleteNotification(notification.id);
      await waitFor(() => workspace.notifications.snapshot().records[notificationKey] === undefined);

      const initial = reduceHostEvent(
        { sessions: {}, notifications: {} },
        { event: "future_additive_event", payload: true },
      );
      expect(reduceHostEvent(initial, { event: "another_future_event" })).toBe(initial);
      expect(Object.isFrozen(workspace.sessions.snapshot())).toBe(true);
    } finally {
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("rolls back optimistic notification updates and preserves typed daemon errors", async () => {
    const archived = notification("n-archived", "archived");
    const daemon = await startTcpFixture({ initialNotifications: [archived] });
    const relay = await relayFor(new Map([["notifications", tcpTarget(daemon)]]));
    const workspace = workspaceFor(relay, [host("notifications")]);
    const key = hostResourceKey("notifications", archived.id);
    const observedStatuses: string[] = [];
    const unsubscribe = workspace.notifications.subscribe((snapshot): void => {
      const status = snapshot.records[key]?.notification.status;
      if (status !== undefined) {
        observedStatuses.push(status);
      }
    });

    try {
      await waitFor(() => connectionKind(workspace, "notifications") === "connected");
      const error = await expectClientError(
        workspace.actions.notificationUpdate("notifications", {
          id: archived.id,
          status: "read",
        }),
      );

      expect(error.kind).toBe("remoteProtocol");
      expect(error.toProtocolError().code).toBe("invalid_notification_transition");
      expect(workspace.notifications.snapshot().records[key]?.notification.status).toBe("archived");
      expect(observedStatuses.includes("read")).toBe(true);
      expect(observedStatuses.at(-1)).toBe("archived");
    } finally {
      unsubscribe();
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("loads every notification cursor page into one immutable snapshot", async () => {
    const initialNotifications = Array.from(
      { length: PAGINATED_NOTIFICATION_COUNT },
      (_, index) => notification(`n-page-${index}`, "unread"),
    );
    const daemon = await startTcpFixture({ initialNotifications });
    const relay = await relayFor(new Map([["pages", tcpTarget(daemon)]]));
    const workspace = workspaceFor(relay, [host("pages")]);

    try {
      await waitFor(() => connectionKind(workspace, "pages") === "connected");
      expect(Object.keys(workspace.notifications.snapshot().records).length)
        .toBe(PAGINATED_NOTIFICATION_COUNT);
      expect(workspace.notifications.snapshot().unreadCount).toBe(PAGINATED_NOTIFICATION_COUNT);
    } finally {
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("supports independent binary attaches, resize, detach, and reattach", async () => {
    const daemon = await startTcpFixture({});
    const relay = await relayFor(new Map([["attach", tcpTarget(daemon)]]));
    const workspace = workspaceFor(relay, [host("attach")]);

    try {
      await waitFor(() => connectionKind(workspace, "attach") === "connected");
      const created = await workspace.actions.sessionNew("attach", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const [first, second] = await Promise.all([
        workspace.attach("attach", created.id),
        workspace.attach("attach", created.id),
      ]);
      expect(first.streamId === second.streamId).toBe(false);
      await expectBinaryRoundTrip(first.stream, NON_UTF8_PAYLOAD);
      await expectBinaryRoundTrip(second.stream, NON_UTF8_PAYLOAD);

      await workspace.actions.sessionResize("attach", {
        session_id: created.id,
        cols: RESIZED_COLS,
        rows: RESIZED_ROWS,
      });
      expect(daemon.scenario.resizes(created.id)).toEqual([{
        cols: RESIZED_COLS,
        rows: RESIZED_ROWS,
      }]);

      await first.detach();
      expect((await workspace.actions.sessionInspect("attach", created.id)).state).toBe("running");
      await second.detach();

      const reattached = await workspace.attach("attach", created.id);
      await expectBinaryRoundTrip(reattached.stream, NON_UTF8_PAYLOAD);
      await reattached.detach();
      expect((await workspace.actions.sessionInspect("attach", created.id)).state).toBe("running");
    } finally {
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });

  test("default hosts source reads the backend hosts endpoint", async () => {
    const expected = [host("from-api")];
    const relay = await startRelay({
      bindHost: LOOPBACK_HOST,
      port: 0,
      targets: new Map(),
      allowLoopbackBind: true,
      httpHandler: (): Response => Response.json(expected),
    });
    try {
      expect(await createBackendHostsSource(relay.url)()).toEqual(expected);
    } finally {
      await relay.close();
    }
  });
  test("dispatches typed session and project parity actions and reconciles their events", async () => {
    const daemon = await startTcpFixture({});
    const relay = await relayFor(new Map([["parity", tcpTarget(daemon)]]));
    const workspace = workspaceFor(relay, [host("parity")]);
    try {
      await waitFor(() => connectionKind(workspace, "parity") === "connected");
      const created = await workspace.actions.sessionNew("parity", { agent: "codex", cols: TEST_COLS, rows: TEST_ROWS });
      await workspace.actions.sessionRename("parity", { session_id: created.id, name: "Renamed" });
      await workspace.actions.sessionSetMetadata("parity", { session_id: created.id, metadata: { work_item: "ABC-1" } });
      const fork = await workspace.actions.sessionFork("parity", { session_id: created.id, cwd_mode: "same", cols: RESIZED_COLS, rows: RESIZED_ROWS });
      await waitFor(() => workspace.sessions.snapshot()[hostResourceKey("parity", fork.id)] !== undefined);
      await workspace.actions.sessionStop("parity", created.id);
      await workspace.actions.sessionResume("parity", created.id);
      await workspace.actions.sessionRemove("parity", created.id);
      await waitFor(() => workspace.sessions.snapshot()[hostResourceKey("parity", created.id)] === undefined);

      const project = await workspace.actions.projectAdd("parity", { path: "/tmp/project", name: "Project" });
      expect((await workspace.actions.projectList("parity", {})).some((item) => item.id === project.id)).toBe(true);
      await workspace.actions.projectRename("parity", { reference: project.id, name: "Renamed project" });
      expect((await workspace.actions.projectShow("parity", { reference: project.id })).project.label).toBe("Renamed project");
      expect((await workspace.actions.projectRemove("parity", { reference: project.id, prune_worktrees: false })).removed).toBe(true);
    } finally {
      await workspace.close();
      await relay.close();
      await daemon.close();
    }
  });
});

interface StartTcpFixtureOptions {
  readonly port?: number;
  readonly protocolVersion?: number;
  readonly initialSessions?: readonly SessionInfo[];
  readonly initialNotifications?: readonly NotificationRecord[];
}

function startTcpFixture(options: StartTcpFixtureOptions): Promise<FixtureDaemonHandle> {
  return startFixtureDaemon({
    listen: { tcp: { host: LOOPBACK_HOST, port: options.port ?? 0 } },
    ...(options.protocolVersion === undefined ? {} : { protocolVersion: options.protocolVersion }),
    ...(options.initialSessions === undefined ? {} : { initialSessions: options.initialSessions }),
    ...(options.initialNotifications === undefined ? {} : { initialNotifications: options.initialNotifications }),
  });
}

function relayFor(targets: ReadonlyMap<string, DaemonTarget>): Promise<RelayHandle> {
  return startRelay({
    bindHost: LOOPBACK_HOST,
    port: 0,
    targets,
    allowLoopbackBind: true,
  });
}

function tcpTarget(daemon: FixtureDaemonHandle): DaemonTarget {
  const address = requireTcpAddress(daemon);
  return { kind: "tcp", host: address.host, port: address.port };
}

function requireTcpAddress(daemon: FixtureDaemonHandle): { readonly host: string; readonly port: number } {
  const address = daemon.tcpAddress;
  if (address === undefined) {
    throw new Error("fixture daemon did not expose a TCP address");
  }
  return address;
}

function workspaceFor(relay: RelayHandle, hosts: readonly HostDescriptor[]): Workspace {
  return createWorkspace({
    baseUrl: relay.url,
    hosts: (): Promise<readonly HostDescriptor[]> => Promise.resolve(hosts),
  });
}

function host(name: string): HostDescriptor {
  return {
    host: name,
    reachability: "reachable_daemon",
    daemon_version: "0.0.0-testkit",
    protocol_version: PROTOCOL_VERSION,
  };
}

function session(id: string, runtimeId?: string): SessionInfo {
  return {
    id,
    agent: "codex",
    agent_base: "codex",
    cwd: "/tmp/pohunek-client-core-test",
    cwd_source: "launch",
    pid: 42_000,
    cols: TEST_COLS,
    rows: TEST_ROWS,
    state: "running",
    state_source: "process",
    activity: "idle",
    created_at: "2026-07-22T12:00:00Z",
    updated_at: "2026-07-22T12:00:00Z",
    ...(runtimeId === undefined ? {} : {
      runtime: {
        state: "live",
        worker_id: `worker-${runtimeId}`,
        runtime_id: runtimeId,
        started_at: "2026-07-22T12:00:00Z",
        last_connected_at: "2026-07-22T12:00:00Z",
      },
    }),
  };
}

function notification(id: string, status: NotificationRecord["status"]): NotificationRecord {
  return {
    id,
    source: {
      provider: "pohunek-testkit",
      provider_event: "client-core-test",
      host_local_source_id: id,
    },
    kind: "system",
    severity: "info",
    status,
    title: "Client core test",
    body: "Notification fixture",
    created_at: "2026-07-22T12:00:00Z",
  };
}

function notificationInput(id: string): Parameters<FixtureDaemonHandle["scenario"]["createNotification"]>[0] {
  return notification(id, "unread");
}

function connectionKind(workspace: Workspace, hostName: string): string | undefined {
  return workspace.hosts.snapshot()[hostName]?.connection.kind;
}

function sessionIds(workspace: Workspace, hostName: string): string[] {
  return Object.values(workspace.sessions.snapshot())
    .filter((entry) => entry.host === hostName)
    .map((entry) => entry.session.id)
    .sort();
}

async function expectClientError(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    if (error instanceof ClientError) {
      return error;
    }
    throw error;
  }
  throw new Error("expected promise to reject with ClientError");
}

async function expectBinaryRoundTrip(stream: RawStream, payload: Uint8Array): Promise<void> {
  const reader = stream.readable.getReader();
  const writer = stream.writable.getWriter();
  try {
    expectBytes(await readExactly(reader, DEFAULT_PTY_READY_BYTES.byteLength), DEFAULT_PTY_READY_BYTES);
    await writer.write(payload);
    expectBytes(await readExactly(reader, payload.byteLength), payload);
  } finally {
    writer.releaseLock();
    reader.releaseLock();
  }
}

async function readExactly(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  byteLength: number,
): Promise<Uint8Array> {
  const output = new Uint8Array(byteLength);
  let offset = 0;
  while (offset < byteLength) {
    const next = await reader.read();
    if (next.done === true) {
      throw new Error(`stream ended after ${offset} bytes; expected ${byteLength}`);
    }
    const remaining = byteLength - offset;
    output.set(next.value.subarray(0, remaining), offset);
    offset += Math.min(next.value.byteLength, remaining);
  }
  return output;
}

function expectBytes(actual: Uint8Array, expected: Uint8Array): void {
  expect(Array.from(actual)).toEqual(Array.from(expected));
}

async function waitFor(condition: () => boolean): Promise<void> {
  const deadline = Date.now() + TEST_TIMEOUT_MS;
  while (!condition()) {
    if (Date.now() >= deadline) {
      throw new Error(`condition timed out after ${TEST_TIMEOUT_MS}ms`);
    }
    await new Promise((resolve) => setTimeout(resolve, TEST_POLL_INTERVAL_MS));
  }
}
