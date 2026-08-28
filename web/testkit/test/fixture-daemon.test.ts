import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createConnection, type Socket } from "node:net";
import { describe, expect, test } from "bun:test";
import {
  MAX_CONTROL_LINE_BYTES,
  MAX_SESSION_OUTPUT_BYTES,
  MAX_SESSION_WAIT_MS,
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type HostRecord,
  type ProtocolEvent,
  type SessionOutputParams,
  type SessionWaitParams,
} from "@pohunek/protocol";
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
const ABOVE_MAX_SAFE_U64 = "9007199254740993";
const U64_MAX_WIRE = "18446744073709551615";
const U64_OVERFLOW_WIRE = "18446744073709551616";

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

  test("models Hermes capability boundaries and rejects unknown launch agents", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("hermes-capabilities") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const inspected = await client.call("host.inspect", null);
      expect(inspected.supported_agents).toContain("hermes");
      expect(inspected.runtimes.find((runtime) => runtime.agent === "hermes")).toEqual({
        agent: "hermes",
        agent_base: "hermes",
        available: true,
        version: "0.20.0",
        supported: true,
      });

      const hermes = await client.call("session.new", {
        agent: "hermes",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      expect(hermes.capabilities).toEqual({ resume: true, fork: false });
      expect(await client.call("session.report_native_id", {
        session_id: hermes.id,
        runtime_id: `runtime-${hermes.id}`,
        agent: "hermes",
        pid: hermes.pid,
        pid_start_identity: "fixture-start-identity",
        sequence: "1",
        expires_at: "2099-08-04T00:00:00Z",
        native_session_id: "hermes-native-session",
      })).toEqual({ recorded: true });
      await client.call("session.stop", hermes.id);
      await client.call("session.resume", hermes.id);

      const sessionIdsBeforeFork = (await client.call("session.list", {})).map((session) => session.id);
      const forkError = await expectClientError(client.call("session.fork", {
        session_id: hermes.id,
        cwd_mode: "same",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      }));
      expect(forkError.toProtocolError()).toEqual({
        class: "runtime",
        code: "agent_fork_unsupported",
        msg: "the selected agent does not support fork",
      });
      expect((await client.call("session.list", {})).map((session) => session.id)).toEqual(sessionIdsBeforeFork);

      const missingReference = await client.call("session.new", {
        agent: "hermes",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      await client.call("session.stop", missingReference.id);
      const missingReferenceError = await expectClientError(client.call("session.resume", missingReference.id));
      expect(missingReferenceError.toProtocolError().code).toBe("not_resumable");
      expect((await client.call("session.inspect", missingReference.id)).state).toBe("stopped");

      const unknownError = await expectClientError(client.call("session.new", {
        agent: "future-agent",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      }));
      expect(unknownError.toProtocolError().code).toBe("agent_kind_unsupported");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("resolves Hermes profile capabilities from runtime inventory", async () => {
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("hermes-profile") },
      host: {
        capabilities: {
          daemon_version: "0.0.0-testkit-hermes-profile",
          protocol_version: PROTOCOL_VERSION,
          supported_agents: ["hermes-work"],
          runtimes: [{
            agent: "hermes-work",
            agent_base: "hermes",
            available: true,
            version: "0.20.0",
            supported: true,
          }],
          git_available: true,
          worktree_supported: true,
          terminal_read_supported: true,
          output_read_supported: true,
          session_wait_supported: true,
        },
      },
    });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "hermes-work",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      expect(created.agent).toBe("hermes-work");
      expect(created.agent_base).toBe("hermes");
      expect(created.capabilities).toEqual({ resume: true, fork: false });
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("rejects missing and unsupported Hermes runtimes without creating sessions", async () => {
    const cases = [
      { name: "bare-missing", agent: "hermes", available: false, supported: undefined },
      { name: "bare-wrong", agent: "hermes", available: true, supported: false },
      { name: "profile-missing", agent: "hermes-work", available: false, supported: true },
      { name: "profile-unproven", agent: "hermes-work", available: true, supported: undefined },
    ] as const;
    for (const testCase of cases) {
      const daemon = await startFixtureDaemon({
        listen: { unixSocketPath: testSocketPath(`hermes-runtime-${testCase.name}`) },
        host: {
          capabilities: {
            daemon_version: "0.0.0-testkit-hermes-runtime",
            protocol_version: PROTOCOL_VERSION,
            supported_agents: [testCase.agent],
            runtimes: [{
              agent: testCase.agent,
              agent_base: "hermes",
              available: testCase.available,
              ...(testCase.supported === undefined ? {} : { supported: testCase.supported }),
            }],
            git_available: true,
            worktree_supported: true,
            terminal_read_supported: true,
            output_read_supported: true,
            session_wait_supported: true,
          },
        },
      });
      try {
        const client = await connectLocal(requireUnixSocket(daemon));
        const error = await expectClientError(client.call("session.new", {
          agent: testCase.agent,
          cols: TEST_COLS,
          rows: TEST_ROWS,
        }));
        expect(error.toProtocolError()).toEqual({
          class: "runtime",
          code: "agent_runtime_unsupported",
          msg: "the selected agent runtime is unavailable or incompatible with this daemon",
        });
        expect(await client.call("session.list", {})).toEqual([]);
        await client.close();
      } finally {
        await daemon.close();
      }
    }
  });

  test("rejects every unknown-agent mutation without side effects", async () => {
    const futureSession = {
      id: "s-future-agent",
      capabilities: { resume: true, fork: true },
      agent: "future-agent",
      agent_base: "future-agent",
      cwd: "/tmp/pohunek-testkit/future-agent",
      pid: 42_500,
      cols: TEST_COLS,
      rows: TEST_ROWS,
      state: "running",
      state_source: "process",
      activity: "idle",
      created_at: "2026-08-04T00:00:00Z",
      updated_at: "2026-08-04T00:00:00Z",
    } as const;
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("future-agent-capabilities") },
      initialSessions: [futureSession],
    });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const before = await client.call("session.inspect", futureSession.id);
      const mutations = [
        (): Promise<unknown> => client.call("session.stop", futureSession.id),
        (): Promise<unknown> => client.call("session.resume", futureSession.id),
        (): Promise<unknown> => client.call("session.remove", futureSession.id),
        (): Promise<unknown> => client.call("session.fork", {
          session_id: futureSession.id,
          cwd_mode: "same",
          cols: RESIZED_COLS,
          rows: RESIZED_ROWS,
        }),
        (): Promise<unknown> => client.call("session.resize", {
          session_id: futureSession.id,
          cols: RESIZED_COLS,
          rows: RESIZED_ROWS,
        }),
        (): Promise<unknown> => client.call("session.set_metadata", {
          session_id: futureSession.id,
          metadata: { changed: "true" },
        }),
        (): Promise<unknown> => client.call("session.rename", {
          session_id: futureSession.id,
          name: "Mutated future session",
        }),
        (): Promise<unknown> => client.call("session.input", {
          session_id: futureSession.id,
          text: "must not be delivered",
        }),
      ];
      for (const mutate of mutations) {
        const error = await expectClientError(mutate());
        expect(error.toProtocolError().code).toBe("agent_kind_unsupported");
      }
      expect(await client.call("session.inspect", futureSession.id)).toEqual(before);
      expect((await client.call("session.list", {})).map((session) => session.id)).toEqual([futureSession.id]);
      expect(daemon.scenario.resizes(futureSession.id)).toEqual([]);
      await client.close();
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
          v: SUPPORTED_PROTOCOL_VERSIONS,
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

  test("fixture serves screen, retained output gaps, bounded waits, and provider policies", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("observe") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const session = await client.call("session.new", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
        metadata: {},
      });
      daemon.scenario.setRetainedOutput(
        session.id,
        new TextEncoder().encode("retained"),
        4,
        "runtime-observe-2",
      );

      const screen = await client.sessionScreen({ session_id: session.id });
      expect(screen.visible_lines).toEqual(["retained"]);
      expect(screen.runtime_id).toBe("runtime-observe-2");

      const output = await client.sessionOutput({
        session_id: session.id,
        runtime: { runtime_id: screen.runtime_id, runtime_generation: screen.runtime_generation },
        after_offset: "2",
        max_bytes: 128,
      });
      expect(output.gap).toEqual({ start_offset: "2", end_offset: "4" });
      expect(Buffer.from(output.data_base64, "base64").toString("utf8")).toBe("retained");

      const waited = await client.sessionWait({
        session_id: session.id,
        runtime: { runtime_id: "stale-runtime", runtime_generation: "1" },
        timeout_ms: 50,
      });
      expect(waited.reason).toBe("runtime_changed");

      const runtime = {
        runtime_id: screen.runtime_id,
        runtime_generation: screen.runtime_generation,
      };
      const invalidOutputCases: readonly SessionOutputParams[] = [
        { session_id: session.id, after_offset: "4", max_bytes: 1 },
        { session_id: session.id, wait_ms: 1, max_bytes: 1 },
        { session_id: session.id, max_bytes: MAX_SESSION_OUTPUT_BYTES + 1 },
        { session_id: session.id, runtime, after_offset: U64_OVERFLOW_WIRE, max_bytes: 1 },
      ];
      for (const params of invalidOutputCases) {
        await expectProtocolError(client.sessionOutput(params), "bad_request");
      }
      await expectProtocolError(client.sessionOutput({
        session_id: session.id,
        runtime,
        after_offset: "999",
        max_bytes: 1,
      }), "session_terminal_unavailable");

      const invalidWaitCases: readonly SessionWaitParams[] = [
        { session_id: session.id, timeout_ms: 1 },
        { session_id: session.id, states: [], timeout_ms: 1 },
        { session_id: session.id, after_output_offset: "4", timeout_ms: 1 },
        { session_id: session.id, runtime, timeout_ms: MAX_SESSION_WAIT_MS + 1 },
        { session_id: session.id, runtime, after_terminal_watermark: U64_OVERFLOW_WIRE, timeout_ms: 1 },
        { session_id: session.id, runtime, after_output_offset: U64_OVERFLOW_WIRE, timeout_ms: 1 },
        { session_id: session.id, runtime: {
          runtime_id: runtime.runtime_id,
          runtime_generation: U64_OVERFLOW_WIRE,
        }, timeout_ms: 1 },
        { session_id: session.id, runtime: {
          runtime_id: "r".repeat(129),
          runtime_generation: "1",
        }, timeout_ms: 1 },
        { session_id: session.id, runtime: {
          runtime_id: "runtime\u0000control",
          runtime_generation: "1",
        }, timeout_ms: 1 },
      ];
      for (const params of invalidWaitCases) {
        await expectProtocolError(client.sessionWait(params), "bad_request");
      }
      await expectProtocolError(client.sessionWait({
        session_id: session.id,
        runtime,
        after_output_offset: "999",
        timeout_ms: 1,
      }), "session_terminal_unavailable");

      const screenWithExtra = { session_id: session.id, unexpected: true };
      await expectProtocolError(client.sessionScreen(screenWithExtra), "bad_request");
      const outputWithExtra = {
        session_id: session.id,
        max_bytes: 1,
        unexpected: true,
      };
      await expectProtocolError(client.sessionOutput(outputWithExtra), "bad_request");
      const waitWithExtra: SessionWaitParams & { readonly unexpected: boolean } = {
        session_id: session.id,
        states: ["running"],
        timeout_ms: 1,
        unexpected: true,
      };
      await expectProtocolError(client.sessionWait(waitWithExtra), "bad_request");
      const runtimeWithExtra = {
        ...runtime,
        unexpected: true,
      };
      await expectProtocolError(client.sessionWait({
        session_id: session.id,
        runtime: runtimeWithExtra,
        timeout_ms: 1,
      }), "bad_request");

      const largeWatermarkWait = await client.sessionWait({
        session_id: session.id,
        runtime,
        after_terminal_watermark: ABOVE_MAX_SAFE_U64,
        timeout_ms: 1,
      });
      expect(largeWatermarkWait.reason).toBe("timeout");
      const maxWatermarkWait = await client.sessionWait({
        session_id: session.id,
        runtime,
        after_terminal_watermark: U64_MAX_WIRE,
        timeout_ms: 1,
      });
      expect(maxWatermarkWait.reason).toBe("timeout");
      const largeGenerationWait = await client.sessionWait({
        session_id: session.id,
        runtime: {
          runtime_id: runtime.runtime_id,
          runtime_generation: ABOVE_MAX_SAFE_U64,
        },
        timeout_ms: 1,
      });
      expect(largeGenerationWait.reason).toBe("runtime_changed");
      const maxLengthRuntimeWait = await client.sessionWait({
        session_id: session.id,
        runtime: { runtime_id: "r".repeat(128), runtime_generation: "1" },
        timeout_ms: 1,
      });
      expect(maxLengthRuntimeWait.reason).toBe("runtime_changed");

      daemon.scenario.setRetainedOutput(
        session.id,
        new TextEncoder().encode("wide"),
        BigInt(ABOVE_MAX_SAFE_U64),
        runtime.runtime_id,
      );
      const largeOutput = await client.sessionOutput({
        session_id: session.id,
        runtime,
        after_offset: (BigInt(ABOVE_MAX_SAFE_U64) + 1n).toString(),
        max_bytes: 16,
      });
      expect(largeOutput.history_start_offset).toBe(ABOVE_MAX_SAFE_U64);
      expect(largeOutput.start_offset).toBe((BigInt(ABOVE_MAX_SAFE_U64) + 1n).toString());
      expect(Buffer.from(largeOutput.data_base64, "base64").toString("utf8")).toBe("ide");
      let overflowError: unknown;
      try {
        daemon.scenario.setRetainedOutput(
          session.id,
          Uint8Array.of(1),
          BigInt(U64_MAX_WIRE),
        );
      } catch (error: unknown) {
        overflowError = error;
      }
      expect(overflowError).toBeInstanceOf(Error);
      if (!(overflowError instanceof Error)) {
        throw new Error("expected retained output overflow error");
      }
      expect(overflowError.message).toContain("exceeds u64");

      const currentPolicy = await client.call("notification.policy.get", null);
      const futurePolicy = { ...currentPolicy.policy.enabled, system: false };
      const updatedPolicy = await client.call("notification.policy.set", {
        policy: {
          ...currentPolicy.policy,
          providers: { ...currentPolicy.policy.providers, "future-agent": futurePolicy },
        },
      });
      expect(updatedPolicy.policy.providers?.["future-agent"]).toEqual(futurePolicy);
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
        address: "100.64.0.2",
        port: 18_722,
        overlay: "netbird",
        peer_id: "peer-a-id",
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

async function expectProtocolError(promise: Promise<unknown>, code: string): Promise<void> {
  const error = await promise.catch((caught: unknown) => caught);
  expect(error).toBeInstanceOf(ClientError);
  expect((error as ClientError).toProtocolError().code).toBe(code);
}

function testSocketPath(label: string): string {
  return join(tmpdir(), `pohunek-testkit-${process.pid}-${label}-${randomUUID()}.sock`);
}

function subscribeRequest(id: string): Request {
  return {
    v: SUPPORTED_PROTOCOL_VERSIONS,
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
