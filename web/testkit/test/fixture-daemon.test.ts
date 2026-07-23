import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createConnection, type Socket } from "node:net";
import { describe, expect, test } from "bun:test";
import { MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION, type HostRecord, type ProtocolEvent } from "@pohunek/protocol";
import {
  ClientError,
  attachRawLocal,
  attachRawTcp,
  connectLocal,
  connectTcp,
  type RawStream,
  type Request,
  type Subscription,
} from "@pohunek/sdk";
import { DEFAULT_PTY_READY_BYTES, startFixtureDaemon, type FixtureDaemonHandle } from "@pohunek/testkit";

const TEST_TCP_HOST = "127.0.0.1";
const TEST_COLS = 80;
const TEST_ROWS = 24;
const RESIZED_COLS = 100;
const RESIZED_ROWS = 30;
const MISMATCHED_PROTOCOL_VERSION = PROTOCOL_VERSION + 1;
const SOCKET_END_TIMEOUT_MS = 5_000;
const NON_UTF8_PAYLOAD = Uint8Array.of(0x00, 0xff, 0x80, 0x61, 0xc3, 0x28);

describe("@pohunek/testkit fixture daemon", () => {
  test("state transitions emit real session and agent events", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("events") } });
    try {
      const socketPath = requireUnixSocket(daemon);
      const subscriber = await connectLocal(socketPath);
      const subscription = await subscriber.subscribe(subscribeRequest("sub-events"));
      const client = await connectLocal(socketPath);

      const created = await client.call("session.new", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const createdEvent = await nextEvent(subscription);
      expect(createdEvent.event).toBe("session_created");
      if (createdEvent.event === "session_created") {
        expect(createdEvent.session.id).toBe(created.id);
        expect(createdEvent.session.state).toBe("running");
      }

      const resized = await client.call("session.resize", {
        session_id: created.id,
        cols: RESIZED_COLS,
        rows: RESIZED_ROWS,
      });
      expect(resized.session.cols).toBe(RESIZED_COLS);
      expect(daemon.scenario.resizes(created.id)).toEqual([{ cols: RESIZED_COLS, rows: RESIZED_ROWS }]);
      expect(daemon.scenario.resizes("s-not-resized")).toEqual([]);
      const updatedEvent = await nextEvent(subscription);
      expect(updatedEvent.event).toBe("session_updated");
      if (updatedEvent.event === "session_updated") {
        expect(updatedEvent.session.rows).toBe(RESIZED_ROWS);
      }

      daemon.scenario.setAgentState(created.id, "blocked", "report");
      const agentEvent = await nextEvent(subscription);
      expect(agentEvent.event).toBe("agent_state");
      if (agentEvent.event === "agent_state") {
        expect(agentEvent.session_id).toBe(created.id);
        expect(agentEvent.activity).toBe("blocked");
        expect(agentEvent.source).toBe("report");
      }

      const stopped = await client.call("session.stop", created.id);
      expect(stopped.stopped).toBe(true);
      const stoppedEvent = await nextEvent(subscription);
      expect(stoppedEvent.event).toBe("session_stopped");
      if (stoppedEvent.event === "session_stopped") {
        expect(stoppedEvent.session.state).toBe("stopped");
      }

      daemon.scenario.removeSession(created.id);
      const removedEvent = await nextEvent(subscription);
      expect(removedEvent.event).toBe("session_removed");
      if (removedEvent.event === "session_removed") {
        expect(removedEvent.session.id).toBe(created.id);
      }
      const missingSession = await expectClientError(client.call("session.inspect", created.id));
      expect(missingSession.toProtocolError().code).toBe("session_not_found");

      await client.close();
      await subscriber.close();
    } finally {
      await daemon.close();
    }
  });

  test("attach round-trips binary bytes without UTF-8 assumptions", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("binary") } });
    try {
      const socketPath = requireUnixSocket(daemon);
      const client = await connectLocal(socketPath);
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const attach = await client.call("session.attach", { session_id: created.id });
      const raw = await attachRawLocal(socketPath, attach.stream_id);

      await expectRoundTrip(raw, NON_UTF8_PAYLOAD);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("oversized control lines are rejected by closing the socket", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("oversized") } });
    try {
      const socket = await connectUnixRaw(requireUnixSocket(daemon));
      let receivedBytes = 0;
      socket.on("data", (chunk: Buffer): void => {
        receivedBytes += chunk.byteLength;
      });

      const ended = waitForEndOrClose(socket);
      await writeSocket(socket, Buffer.alloc(MAX_CONTROL_LINE_BYTES + 1, 0x61));
      const terminalEvent = await ended;

      expect(receivedBytes).toBe(0);
      expect(terminalEvent === "end" || terminalEvent === "close").toBe(true);
      socket.destroy();
    } finally {
      await daemon.close();
    }
  });

  test("unknown methods return daemon method_not_found", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("unknown") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const error = await expectClientError(
        client.request({
          v: PROTOCOL_VERSION,
          id: "unknown-method",
          method: "session.unknown",
          params: null,
        }),
      );

      const structured = error.toProtocolError();
      expect(structured.class).toBe("daemon");
      expect(structured.code).toBe("method_not_found");
      expect(structured.msg).toBe("unknown control method: session.unknown");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("daemon health reports the configured protocol version", async () => {
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("protocol-version") },
      protocolVersion: MISMATCHED_PROTOCOL_VERSION,
    });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const health = await client.call("daemon.health", null);

      expect(health.protocol_version).toBe(MISMATCHED_PROTOCOL_VERSION);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("scenario controls drive host discovery and notification events", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("scenario") } });
    try {
      const socketPath = requireUnixSocket(daemon);
      const subscriber = await connectLocal(socketPath);
      const subscription = await subscriber.subscribe(subscribeRequest("sub-scenario"));
      const client = await connectLocal(socketPath);
      const host = {
        name: "peer-a",
        fqdn: "peer-a.example.netbird.cloud",
        netbird_ip: "100.64.0.2",
        classification: "reachable_daemon",
        daemon_version: "0.0.0-testkit",
      } as const satisfies HostRecord;

      daemon.scenario.setDiscoveredHosts([host]);
      expect(await client.call("host.discover", { force: false })).toEqual([host]);

      const created = daemon.scenario.createNotification({
        source: {
          provider: "codex",
          provider_event: "approval_required",
          host_local_source_id: "hook-testkit-1",
        },
        kind: "approval_required",
        severity: "action_required",
        title: "Approval required",
        body: "Codex is waiting for an owner decision.",
      });
      const createdEvent = await nextEvent(subscription);
      expect(createdEvent.event).toBe("notification_created");
      if (createdEvent.event === "notification_created") {
        expect(createdEvent.record.id).toBe(created.id);
        expect(createdEvent.record.status).toBe("unread");
      }

      const listed = await client.call("notification.list", { status: "unread" });
      expect(listed.notifications.length).toBe(1);
      expect(listed.notifications[0]?.id).toBe(created.id);

      const updated = await client.call("notification.update", {
        id: created.id,
        status: "read",
      });
      expect(updated.record.status).toBe("read");
      const updatedEvent = await nextEvent(subscription);
      expect(updatedEvent.event).toBe("notification_updated");
      if (updatedEvent.event === "notification_updated") {
        expect(updatedEvent.record.id).toBe(created.id);
        expect(updatedEvent.record.status).toBe("read");
      }

      daemon.scenario.deleteNotification(created.id);
      const deletedEvent = await nextEvent(subscription);
      expect(deletedEvent.event).toBe("notification_deleted");
      if (deletedEvent.event === "notification_deleted") {
        expect(deletedEvent.notification_id).toBe(created.id);
      }
      expect((await client.call("notification.list", {})).notifications).toEqual([]);

      await client.close();
      await subscriber.close();
    } finally {
      await daemon.close();
    }
  });

  test("unchanged SDK client runs core scenarios against Unix and TCP listeners", async () => {
    const daemon = await startFixtureDaemon({
      listen: {
        unixSocketPath: testSocketPath("honesty"),
        tcp: { host: TEST_TCP_HOST, port: 0 },
      },
    });
    try {
      const socketPath = requireUnixSocket(daemon);
      const tcpAddress = requireTcpAddress(daemon);
      const localClient = await connectLocal(socketPath);

      expect(await localClient.handshake()).toBe(PROTOCOL_VERSION);

      const created = await localClient.call("session.new", {
        agent: "codex",
        name: "SDK honesty",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const listed = await localClient.call("session.list", {
        filters: [{ key: "id", value: created.id }],
      });
      expect(listed.length).toBe(1);
      expect(listed[0]?.id).toBe(created.id);

      const subscriberClient = await connectTcp("fixture-host", tcpAddress);
      const subscription = await subscriberClient.subscribe(subscribeRequest("sub-honesty"));
      daemon.scenario.setAgentState(created.id, "working", "report");
      const scenarioEvent = await nextEvent(subscription);
      expect(scenarioEvent.event).toBe("agent_state");
      if (scenarioEvent.event === "agent_state") {
        expect(scenarioEvent.session_id).toBe(created.id);
        expect(scenarioEvent.activity).toBe("working");
      }

      const localAttach = await localClient.call("session.attach", { session_id: created.id });
      await expectRoundTrip(await attachRawLocal(socketPath, localAttach.stream_id), NON_UTF8_PAYLOAD);

      const tcpClient = await connectTcp("fixture-host", tcpAddress);
      const tcpAttach = await tcpClient.call("session.attach", { session_id: created.id });
      await expectRoundTrip(await attachRawTcp("fixture-host", tcpAddress, tcpAttach.stream_id), NON_UTF8_PAYLOAD);

      const protocolError = await expectClientError(localClient.call("session.inspect", "s-missing"));
      expect(protocolError.kind).toBe("protocol");
      expect(protocolError.toProtocolError().class).toBe("runtime");
      expect(protocolError.toProtocolError().code).toBe("session_not_found");

      await tcpClient.close();
      await subscriberClient.close();
      await localClient.close();
    } finally {
      await daemon.close();
    }
  });
  test("models session lifecycle parity and project/worktree safeguards", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("parity") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", { agent: "codex", cols: TEST_COLS, rows: TEST_ROWS });
      expect((await client.call("session.rename", { session_id: created.id, name: "Renamed" })).session.name).toBe("Renamed");
      expect((await client.call("session.set_metadata", { session_id: created.id, metadata: { issue: "ABC-1" } })).session.metadata).toEqual({ issue: "ABC-1" });
      const fork = await client.call("session.fork", { session_id: created.id, cwd_mode: "same", cols: RESIZED_COLS, rows: RESIZED_ROWS });
      expect(fork.cols).toBe(RESIZED_COLS);
      await client.call("session.stop", created.id);
      expect((await client.call("session.resume", created.id)).session.state).toBe("running");
      expect(await client.call("session.remove", created.id)).toEqual({ removed: true, stopped: true });

      const project = await client.call("project.add", { path: "/tmp/test-project", name: "Test project", base_branch: "main" });
      expect((await client.call("project.show", { reference: project.id })).project.label).toBe("Test project");
      const shown = await client.call("project.show", { reference: project.id });
      const protectedWorktree = await expectClientError(client.call("worktree.remove", { path: shown.worktrees[0]?.path ?? "" }));
      expect(protectedWorktree.toProtocolError().code).toBe("bad_request");
      expect((await client.call("project.rename", { reference: project.id, name: "Renamed project" })).label).toBe("Renamed project");
      expect((await client.call("project.remove", { reference: project.id, prune_worktrees: true })).removed).toBe(true);
      await client.close();
    } finally {
      await daemon.close();
    }
  });
});

function testSocketPath(label: string): string {
  return join(tmpdir(), `pohunek-testkit-${process.pid}-${label}-${randomUUID()}.sock`);
}

function subscribeRequest(id: string): Request {
  return {
    v: PROTOCOL_VERSION,
    id,
    method: "subscribe",
    params: null,
  };
}

function requireUnixSocket(daemon: FixtureDaemonHandle): string {
  const socketPath = daemon.unixSocketPath;
  if (socketPath === undefined) {
    throw new Error("fixture daemon did not expose a Unix socket");
  }
  return socketPath;
}

function requireTcpAddress(daemon: FixtureDaemonHandle): { readonly host: string; readonly port: number } {
  const address = daemon.tcpAddress;
  if (address === undefined) {
    throw new Error("fixture daemon did not expose a TCP address");
  }
  return address;
}

async function nextEvent(subscription: Subscription): Promise<ProtocolEvent> {
  const event = await subscription.nextEvent();
  if (event === null) {
    throw new Error("expected subscription event, got end of stream");
  }
  if (!isKnownProtocolEvent(event)) {
    throw new Error(`expected a known protocol event, got ${event.event}`);
  }
  return event;
}

async function expectRoundTrip(raw: RawStream, payload: Uint8Array): Promise<void> {
  const reader = raw.readable.getReader();
  const writer = raw.writable.getWriter();
  try {
    expectBytes(await readExactly(reader, DEFAULT_PTY_READY_BYTES.byteLength), DEFAULT_PTY_READY_BYTES);
    await writer.write(payload);
    expectBytes(await readExactly(reader, payload.byteLength), payload);
  } finally {
    writer.releaseLock();
    reader.releaseLock();
    await raw.close();
  }
}

async function readExactly(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  byteLength: number,
): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (total < byteLength) {
    const next = await reader.read();
    if (next.done === true) {
      throw new Error(`stream ended after ${total} bytes; expected ${byteLength}`);
    }
    chunks.push(next.value);
    total += next.value.byteLength;
  }

  const output = new Uint8Array(byteLength);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk.subarray(0, Math.min(chunk.byteLength, byteLength - offset)), offset);
    offset += chunk.byteLength;
    if (offset >= byteLength) {
      break;
    }
  }
  return output;
}

function expectBytes(actual: Uint8Array, expected: Uint8Array): void {
  expect(actual.byteLength).toBe(expected.byteLength);
  for (let index = 0; index < expected.byteLength; index += 1) {
    expect(actual[index]).toBe(expected[index]);
  }
}

function connectUnixRaw(socketPath: string): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const socket = createConnection({ path: socketPath, allowHalfOpen: true });
    const fail = (error: Error): void => {
      socket.off("connect", done);
      reject(error);
    };
    const done = (): void => {
      socket.off("error", fail);
      resolve(socket);
    };
    socket.once("error", fail);
    socket.once("connect", done);
  });
}

function writeSocket(socket: Socket, bytes: Uint8Array): Promise<void> {
  return new Promise((resolve, reject) => {
    socket.write(Buffer.from(bytes), (error?: Error | null) => {
      if (error instanceof Error) {
        reject(error);
        return;
      }
      resolve();
    });
  });
}

async function waitForEndOrClose(socket: Socket): Promise<"end" | "close"> {
  return withTimeout(
    new Promise<"end" | "close">((resolve) => {
      socket.once("end", (): void => {
        resolve("end");
      });
      socket.once("close", (): void => {
        resolve("close");
      });
    }),
    SOCKET_END_TIMEOUT_MS,
    "socket end",
  );
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, action: string): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`${action} timed out after ${timeoutMs}ms`));
    }, timeoutMs);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error instanceof Error ? error : new Error(String(error)));
      },
    );
  });
}

async function expectClientError(promise: Promise<unknown>): Promise<ClientError> {
  try {
    await promise;
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(ClientError);
    return error as ClientError;
  }
  throw new Error("expected promise to reject with ClientError");
}

function isKnownProtocolEvent(event: Awaited<ReturnType<Subscription["nextEvent"]>>): event is ProtocolEvent {
  if (event === null) {
    return false;
  }
  return (
    event.event === "agent_state" ||
    event.event === "attach_closed" ||
    event.event === "attach_opened" ||
    event.event === "notification_created" ||
    event.event === "notification_deleted" ||
    event.event === "notification_updated" ||
    event.event === "session_created" ||
    event.event === "session_removed" ||
    event.event === "session_stopped" ||
    event.event === "session_updated"
  );
}
