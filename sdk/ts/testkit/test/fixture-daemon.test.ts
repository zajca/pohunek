import { Buffer } from "node:buffer";
import { rmSync } from "node:fs";
import { join } from "node:path";
import { createConnection, type Socket } from "node:net";
import { describe, expect, test } from "bun:test";
import {
  MAX_CONTROL_LINE_BYTES,
  MAX_SESSION_OUTPUT_BYTES,
  MAX_SESSION_WAIT_MS,
  PROTOCOL_VERSION,
  CLIENT_PROTOCOL_VERSIONS,
  type HostRecord,
  type ProtocolEvent,
  type AgentActivity,
  type SessionInputResult,
  type SessionOutputParams,
  type SessionTurnEventKind,
  type SessionWaitParams,
  type SessionWaitTurnParams,
  type SessionWaitTurnResult,
} from "@pohunek/protocol";
import {
  ClientError,
  attachRawLocal,
  attachRawTcp,
  connectLocal,
  connectTcp,
  type Client,
  type RawStream,
  type Request,
  type Subscription,
} from "@pohunek/sdk";
import {
  DEFAULT_PTY_READY_BYTES,
  createFixtureRootSync,
  startFixtureDaemon,
  type FixtureDaemonHandle,
} from "@pohunek/testkit";

const TEST_TCP_HOST = "127.0.0.1";
const TEST_COLS = 80;
const TEST_ROWS = 24;
const RESIZED_COLS = 100;
const RESIZED_ROWS = 30;
const MISMATCHED_PROTOCOL_VERSION = PROTOCOL_VERSION + 1;
const SOCKET_END_TIMEOUT_MS = 5_000;
const INPUT_WAITER_POLL_INTERVAL_MS = 1;
const ALL_TURN_KINDS: SessionTurnEventKind[] = ["turn_completed", "agent_blocked", "approval_required", "exited"];
// Long enough that a wait expected to complete on an event never times out first.
const TURN_EVENT_GUARD_MS = 30_000;
// Short fixture-side waits whose expected outcome is a timeout; the test
// asserts only the reason and progress, never the elapsed time.
const SHORT_TURN_WAIT_MS = 20;
// A client response deadline far below the turn waits it must not cut short.
const SHORT_REQUEST_TIMEOUT_MS = 200;
const LONG_TURN_WAIT_MS = SHORT_REQUEST_TIMEOUT_MS * 3;
const TURN_WAIT_CEILING_MS = LONG_TURN_WAIT_MS * 2;
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

  test("session wait compares its update cursor as an instant", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("wait-cursor") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const stamp = created.updated_at;
      const utcPlusZero = stamp.replace(/Z$/u, "+00:00");
      // The same instant is no update, although the text sorts before the stamp.
      const equal = await client.call("session.wait", {
        session_id: created.id,
        after_updated_at: utcPlusZero,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(equal.reason).toBe("timeout");
      // A cursor one second later, written at -12:00, is no update either.
      const later = new Date(Date.parse(stamp) + 1_000);
      const shifted = new Date(later.getTime() - 12 * 3_600_000).toISOString().replace(/Z$/u, "-12:00");
      const notYet = await client.call("session.wait", {
        session_id: created.id,
        after_updated_at: shifted,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(notYet.reason).toBe("timeout");
      // A cursor one second earlier is.
      const earlier = new Date(Date.parse(stamp) - 1_000).toISOString();
      const updated = await client.call("session.wait", {
        session_id: created.id,
        after_updated_at: earlier,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(updated.reason).toBe("session_updated");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait returns runtime-scoped activity evidence", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-wait") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const result = await inputThenReport(daemon, client, created.id, { until: ["idle"] });

      expect(result.accepted).toBe(true);
      expect(result.activity).toBe("idle");
      expect(result.activity_source).toBe("report");
      expect(result.runtime?.worker_instance_id).toBe(`runtime-${created.id}`);
      expect(result.activity_epoch?.startsWith("d-testkit-")).toBe(true);
      expect(result.activity_revision).toBe("1");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait accepts absent targets as the default target set", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-default-targets") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const result = await inputThenReport(daemon, client, created.id, {});

      expect(result.activity).toBe("idle");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session remove_accepting_unconfirmed removes a session like session.remove with no candidates", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("remove-accepting") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", { agent: "codex", cols: TEST_COLS, rows: TEST_ROWS });

      await expectProtocolError(
        client.call("session.remove_accepting_unconfirmed", { session_id: created.id } as never),
        "bad_request",
      );
      expect(await client.call("session.remove_accepting_unconfirmed", created.id))
        .toEqual({ removed: true, stopped: true, worktrees_removed: 0, worktrees_failed: 0 });
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait on delayed provider framing needs working before idle", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-delayed") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });

      // The fixture holds its waiter before idle is reported, so the first idle
      // (revision 1) is seen and must not satisfy a delayed-submit wait; the
      // wait ends at the idle after working evidence (revision 3).
      const result = await inputThenReport(daemon, client, created.id, { until: ["idle"] }, ["idle", "working", "idle"]);
      expect(result.activity_revision).toBe("3");
      expect(result.activity).toBe("idle");
      expect(result.submitted).toBe(true);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session new reports prompt_delivered for an acknowledged initial input", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("new-ack") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const idle = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
        input: "hello",
        input_ack_timeout_ms: 30,
      });
      expect(idle.applied_input).toBe(true);
      expect(idle.prompt_delivered).toBe(false);

      const plain = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
        input: "hello",
      });
      expect(plain.prompt_delivered).toBeUndefined();

      await expectProtocolError(client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
        input_ack_timeout_ms: 30,
      }), "bad_request");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait returns a typed timeout without matching activity", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-timeout") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });

      const error = await client.call("session.input", {
        session_id: created.id,
        text: "hello",
        wait: { until: ["idle"], timeout_ms: 10 },
      }).catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(ClientError);
      const structured = (error as ClientError).toProtocolError();
      expect(structured.code).toBe("session_input_timeout");
      expect(structured.recover).toContain("inspect the current session");
      expect(structured.recover).toContain("do not retry blindly");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait rejects blocked activity before writing PTY bytes", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-blocked") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      daemon.scenario.setAgentState(created.id, "blocked", "report");

      await expectProtocolError(client.call("session.input", {
        session_id: created.id,
        text: "must-not-write",
        wait: { until: ["idle"], timeout_ms: 200 },
      }), "session_agent_blocked");
      expect(daemon.scenario.inputs(created.id)).toEqual([]);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session input wait wakes on stop, remove, and runtime replacement", async () => {
    for (const lifecycle of ["stop", "remove", "replace"] as const) {
      const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath(`input-${lifecycle}`) } });
      try {
        const client = await connectLocal(requireUnixSocket(daemon));
        const control = await connectLocal(requireUnixSocket(daemon));
        const created = await client.call("session.new", {
          agent: "shell",
          cols: TEST_COLS,
          rows: TEST_ROWS,
        });
        const waiting = client.call("session.input", {
          session_id: created.id,
          text: `wait-${lifecycle}`,
          wait: { until: ["idle"], timeout_ms: 500 },
        });
        await new Promise((resolve) => setTimeout(resolve, 20));
        if (lifecycle === "stop") {
          await control.call("session.stop", created.id);
        } else if (lifecycle === "remove") {
          daemon.scenario.removeSession(created.id);
        } else {
          daemon.scenario.replaceRuntime(created.id);
        }

        const expected = lifecycle === "stop"
          ? "session_not_running"
          : lifecycle === "remove"
            ? "session_not_found"
            : "session_runtime_changed";
        await expectProtocolError(waiting, expected);
        await control.close();
        await client.close();
      } finally {
        await daemon.close();
      }
    }
  });

  test("fixture shutdown cancels active session input waiters", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("input-shutdown") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", {
        agent: "shell",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const waiting = client.call("session.input", {
        session_id: created.id,
        text: "wait for shutdown",
        wait: { until: ["blocked"], timeout_ms: MAX_SESSION_WAIT_MS },
      }).catch((error: unknown) => error);
      await withTimeout(
        waitForActiveInputWaiter(daemon),
        SOCKET_END_TIMEOUT_MS,
        "fixture input waiter registration",
      );
      expect(daemon.activeInputWaiterCount).toBe(1);

      await daemon.close();

      expect(daemon.activeInputWaiterCount).toBe(0);
      expect(await waiting).toBeInstanceOf(ClientError);
      await client.close();
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
        worker_instance_id: `runtime-${hermes.id}`,
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
        expect(error.toProtocolError()).toEqual(testCase.available
          ? {
            class: "runtime",
            code: "agent_runtime_unsupported",
            msg: "the selected agent executable did not pass this daemon's version check",
          }
          : {
            class: "runtime",
            code: "agent_binary_missing",
            msg: "agent binary not found on PATH: hermes",
            recover: "install the hermes CLI and ensure it is on PATH; run `pohunek doctor` to verify",
          });
        expect(await client.call("session.list", {})).toEqual([]);
        await client.close();
      } finally {
        await daemon.close();
      }
    }
  });

  test("rejects a supported agent without a runtime row as an unknown profile", async () => {
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("agent-without-runtime") },
      host: {
        capabilities: {
          daemon_version: "0.0.0-testkit-agent-without-runtime",
          protocol_version: PROTOCOL_VERSION,
          supported_agents: ["hermes-work"],
          runtimes: [],
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
        agent: "hermes-work",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      }));
      expect(error.toProtocolError()).toEqual({
        class: "runtime",
        code: "agent_profile_not_found",
        msg: "no agent profile or base kind named 'hermes-work' on this host",
        recover: "use `shell` or an installed runtime (`pohunek plugin list`), or add ~/.config/pohunek/agents/<name>.toml on the target host",
      });
      expect(await client.call("session.list", {})).toEqual([]);
      await client.close();
    } finally {
      await daemon.close();
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
          v: CLIENT_PROTOCOL_VERSIONS,
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
      expect(screen.worker_instance_id).toBe("runtime-observe-2");

      const output = await client.sessionOutput({
        session_id: session.id,
        runtime: { worker_instance_id: screen.worker_instance_id, runtime_generation: screen.runtime_generation },
        after_offset: "2",
        max_bytes: 128,
      });
      expect(output.gap).toEqual({ start_offset: "2", end_offset: "4" });
      expect(Buffer.from(output.data_base64, "base64").toString("utf8")).toBe("retained");

      const waited = await client.sessionWait({
        session_id: session.id,
        runtime: { worker_instance_id: "stale-runtime", runtime_generation: "1" },
        timeout_ms: 50,
      });
      expect(waited.reason).toBe("runtime_changed");

      const runtime = {
        worker_instance_id: screen.worker_instance_id,
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
          worker_instance_id: runtime.worker_instance_id,
          runtime_generation: U64_OVERFLOW_WIRE,
        }, timeout_ms: 1 },
        { session_id: session.id, runtime: {
          worker_instance_id: "r".repeat(129),
          runtime_generation: "1",
        }, timeout_ms: 1 },
        { session_id: session.id, runtime: {
          worker_instance_id: "runtime\u0000control",
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
          worker_instance_id: runtime.worker_instance_id,
          runtime_generation: ABOVE_MAX_SAFE_U64,
        },
        timeout_ms: 1,
      });
      expect(largeGenerationWait.reason).toBe("runtime_changed");
      const maxLengthRuntimeWait = await client.sessionWait({
        session_id: session.id,
        runtime: { worker_instance_id: "r".repeat(128), runtime_generation: "1" },
        timeout_ms: 1,
      });
      expect(maxLengthRuntimeWait.reason).toBe("runtime_changed");

      daemon.scenario.setRetainedOutput(
        session.id,
        new TextEncoder().encode("wide"),
        BigInt(ABOVE_MAX_SAFE_U64),
        runtime.worker_instance_id,
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
        metadata: { ticket: "ABC-1", team: "runtime" },
      });
      await localClient.call("session.new", {
        agent: "codex",
        cols: TEST_COLS,
        rows: TEST_ROWS,
      });
      const listed = await localClient.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "ABC-1" } }],
      });
      expect(listed.map((session) => session.id)).toEqual([created.id]);
      expect((await localClient.call("session.list", {
        filters: [{ key: "metadata_has", value: { key: "team" } }],
      })).map((session) => session.id)).toEqual([created.id]);
      expect((await localClient.call("session.list", {
        filters: [
          { key: "metadata_has", value: { key: "ticket" } },
          { key: "metadata_eq", value: { key: "team", value: "runtime" } },
          { key: "id", value: created.id },
        ],
      })).map((session) => session.id)).toEqual([created.id]);
      expect(await localClient.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "ABC-2" } }],
      })).toEqual([]);

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

  test("session-list metadata filters enforce daemon byte and serialized-size limits", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("metadata-filter-bounds") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));

      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "metadata_has", value: { key: "x".repeat(65) } }],
      }), "bad_request");
      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "metadata_has", value: { key: "é".repeat(33) } }],
      }), "bad_request");
      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "x".repeat(4097) } }],
      }), "bad_request");
      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "é".repeat(2049) } }],
      }), "bad_request");
      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "\u0000".repeat(3000) } }],
      }), "bad_request");

      expect(await client.call("session.list", {
        filters: [{ key: "metadata_has", value: { key: "é".repeat(32) } }],
      })).toEqual([]);
      expect(await client.call("session.list", {
        filters: [{ key: "metadata_eq", value: { key: "ticket", value: "é".repeat(2048) } }],
      })).toEqual([]);

      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("session-list spawner filter takes a session id and matches the recorded spawner", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("spawned-by-filter") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const created = await client.call("session.new", { agent: "codex", cols: TEST_COLS, rows: TEST_ROWS });
      expect(created.spawned_by).toBeUndefined();

      expect(await client.call("session.list", {
        filters: [{ key: "spawned_by", value: "s-parent" }],
      })).toEqual([]);

      const spawner = await connectLocal(requireUnixSocket(daemon), {
        origin: { sessionId: "s-parent", daemonId: "d-parent" },
      });
      const child = await spawner.call("session.new", { agent: "codex", cols: TEST_COLS, rows: TEST_ROWS });
      expect(child.spawned_by).toEqual({ session_id: "s-parent", daemon_id: "d-parent" });
      expect((await client.call("session.list", {
        filters: [{ key: "spawned_by", value: "s-parent" }],
      })).map((session) => session.id)).toEqual([child.id]);
      await spawner.close();
      await expectProtocolError(client.call("session.list", {
        filters: [{ key: "spawned_by", value: { session_id: "s-parent" } as unknown as string }],
      }), "bad_request");

      await client.close();
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
      expect(await client.call("session.remove", created.id)).toEqual({ removed: true, stopped: true, worktrees_removed: 0, worktrees_failed: 0 });

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

describe("session.wait_turn through the SDK client against the fixture daemon", () => {
  test("host inspection answers only the flags when the runtime probes are skipped", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("skip-probes") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const flags = await client.call("host.inspect", { skip_runtime_probes: true });
      expect(flags.runtimes).toEqual([]);
      expect(flags.supported_agents).toEqual([]);
      expect(flags.session_wait_turn_supported).toBe(true);
      const full = await client.call("host.inspect", { skip_runtime_probes: false });
      expect(full.runtimes.length).toBeGreaterThan(0);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("host inspection advertises turn waits", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-caps") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      expect((await client.call("host.inspect", null)).session_wait_turn_supported).toBe(true);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("chained waits return every event recorded between them and name its session", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-chain") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const first = await newShellSession(client);
      const second = await newShellSession(client);
      const targets = [first, second];

      // Edge-triggered: a turn that ended before the call is never returned.
      daemon.scenario.recordTurnEvent(first, "turn_completed");
      const initial = await client.sessionWaitTurn({
        session_ids: targets,
        until: ALL_TURN_KINDS,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(initial.reason).toBe("timeout");
      expect(initial.event).toBeUndefined();

      // An event recorded while the wait is pending completes it.
      const pending = client.sessionWaitTurn({
        session_ids: targets,
        until: ALL_TURN_KINDS,
        after: initial.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      await waitForTurnWaiters(daemon, 1);
      daemon.scenario.recordTurnEvent(second, "agent_blocked", "n-kept");
      const blocked = await pending;
      expect(blocked.reason).toBe("event");
      expect(eventOf(blocked)).toEqual({ session_id: second, kind: "agent_blocked", notification_id: "n-kept" });
      expect(blocked.progress.map((entry) => entry.session_id)).toEqual(targets);
      expect(daemon.activeTurnWaiterCount).toBe(0);

      // Events recorded between two chained waits come back oldest first.
      daemon.scenario.recordTurnEvent(first, "turn_completed");
      daemon.scenario.recordTurnEvent(second, "approval_required");
      const next = await client.sessionWaitTurn({
        session_ids: targets,
        until: ALL_TURN_KINDS,
        after: blocked.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(next.reason).toBe("event");
      expect(eventOf(next)).toEqual({ session_id: first, kind: "turn_completed", notification_id: undefined });
      const last = await client.sessionWaitTurn({
        session_ids: targets,
        until: ALL_TURN_KINDS,
        after: next.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(last.reason).toBe("event");
      expect(eventOf(last)).toEqual({ session_id: second, kind: "approval_required", notification_id: undefined });

      // A kind outside `until` does not complete the wait, and the timeout
      // cursor covers it so the next wait does not see it again.
      daemon.scenario.recordTurnEvent(first, "agent_blocked");
      const unselected = await client.sessionWaitTurn({
        session_ids: targets,
        until: ["turn_completed"],
        after: last.cursor,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(unselected.reason).toBe("timeout");
      const after = await client.sessionWaitTurn({
        session_ids: targets,
        until: ALL_TURN_KINDS,
        after: unselected.cursor,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(after.reason).toBe("timeout");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("a timeout returns progress for every target", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-progress") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const working = await newShellSession(client);
      const quiet = await newShellSession(client);
      daemon.scenario.setAgentState(working, "working");
      const first = await client.sessionWaitTurn({
        session_ids: [working, quiet],
        until: ["turn_completed"],
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(first.reason).toBe("timeout");

      daemon.scenario.queuePtyOutput(working, Buffer.from("still compiling\n"));
      const timedOut = await client.sessionWaitTurn({
        session_ids: [working, quiet],
        until: ["turn_completed"],
        after: first.cursor,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(timedOut.reason).toBe("timeout");
      expect(timedOut.event).toBeUndefined();
      expect(timedOut.progress.length).toBe(2);
      const [workingProgress, quietProgress] = timedOut.progress;
      expect({
        session_id: workingProgress?.session_id,
        state: workingProgress?.state,
        activity: workingProgress?.activity,
        output_advanced: workingProgress?.output_advanced,
      }).toEqual({
        session_id: working,
        state: "running",
        activity: "working",
        output_advanced: true,
      });
      expectRfc3339(workingProgress?.last_output_at);
      expectRfc3339(workingProgress?.turn_started_at);
      expect(quietProgress).toEqual({
        session_id: quiet,
        state: "running",
        activity: "idle",
        output_advanced: false,
      });

      // Output is "advanced" relative to the cursor, so no new output reads false.
      const again = await client.sessionWaitTurn({
        session_ids: [working, quiet],
        until: ["turn_completed"],
        after: timedOut.cursor,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(again.progress[0]?.output_advanced).toBe(false);
      expect(again.progress[0]?.last_output_at).toBe(workingProgress?.last_output_at);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("exit, ended targets, and unknown or removed targets follow the contract", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-exit") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const exiting = await newShellSession(client);
      const removed = await newShellSession(client);

      const pendingExit = client.sessionWaitTurn({
        session_ids: [exiting],
        until: ["exited"],
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      await waitForTurnWaiters(daemon, 1);
      expect((await client.call("session.stop", exiting)).stopped).toBe(true);
      const exited = await pendingExit;
      expect(exited.reason).toBe("event");
      expect(eventOf(exited)).toEqual({ session_id: exiting, kind: "exited", notification_id: undefined });
      expect(exited.progress[0]?.state).toBe("stopped");

      // Every target terminal and nothing newer selected: the call returns at once.
      const ended = await client.sessionWaitTurn({
        session_ids: [exiting],
        until: ["turn_completed"],
        after: exited.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(ended.reason).toBe("targets_ended");
      expect(ended.event).toBeUndefined();

      const unknown = await expectClientError(client.sessionWaitTurn({
        session_ids: [removed, "s-testkit-missing"],
        until: ALL_TURN_KINDS,
        after: "t1.1.1.d-other-daemon",
      }));
      expect(unknown.toProtocolError().code).toBe("session_not_found");
      expect(daemon.activeTurnWaiterCount).toBe(0);

      const pendingRemoval = client.sessionWaitTurn({
        session_ids: [removed],
        until: ALL_TURN_KINDS,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      await waitForTurnWaiters(daemon, 1);
      daemon.scenario.removeSession(removed);
      const removal = await expectClientError(pendingRemoval);
      expect(removal.toProtocolError().code).toBe("session_not_found");
      expect(daemon.activeTurnWaiterCount).toBe(0);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("a cursor the daemon cannot place resets with a fresh cursor", async () => {
    const other = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-other") } });
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("turn-reset") },
      turnWait: { journalCapacity: 1 },
    });
    try {
      const otherClient = await connectLocal(requireUnixSocket(other));
      const otherSession = await newShellSession(otherClient);
      const foreign = (await otherClient.sessionWaitTurn({
        session_ids: [otherSession],
        until: ALL_TURN_KINDS,
        timeout_ms: SHORT_TURN_WAIT_MS,
      })).cursor;
      await otherClient.close();

      const client = await connectLocal(requireUnixSocket(daemon));
      const session = await newShellSession(client);
      const reset = await client.sessionWaitTurn({
        session_ids: [session],
        until: ALL_TURN_KINDS,
        after: foreign,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(reset.reason).toBe("cursor_reset");
      expect(reset.event).toBeUndefined();
      expect(reset.cursor === foreign).toBe(false);
      expect(reset.progress.map((entry) => entry.session_id)).toEqual([session]);

      // The fresh cursor chains: the next event after it is returned.
      daemon.scenario.recordTurnEvent(session, "turn_completed");
      const chained = await client.sessionWaitTurn({
        session_ids: [session],
        until: ALL_TURN_KINDS,
        after: reset.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(chained.reason).toBe("event");
      expect(eventOf(chained)).toEqual({ session_id: session, kind: "turn_completed", notification_id: undefined });

      // A cursor older than the retained journal window resets too.
      daemon.scenario.recordTurnEvent(session, "agent_blocked");
      daemon.scenario.recordTurnEvent(session, "approval_required");
      const evicted = await client.sessionWaitTurn({
        session_ids: [session],
        until: ALL_TURN_KINDS,
        after: chained.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      expect(evicted.reason).toBe("cursor_reset");

      // A parked wait whose following events are evicted resets too, even when
      // a selected event is still retained after the eviction.
      const parked = client.sessionWaitTurn({
        session_ids: [session],
        until: ["turn_completed"],
        after: evicted.cursor,
        timeout_ms: TURN_EVENT_GUARD_MS,
      });
      await waitForTurnWaiters(daemon, 1);
      daemon.scenario.recordTurnEvent(session, "agent_blocked");
      daemon.scenario.recordTurnEvent(session, "agent_blocked");
      daemon.scenario.recordTurnEvent(session, "turn_completed");
      const parkedReset = await parked;
      expect(parkedReset.reason).toBe("cursor_reset");
      expect(parkedReset.event).toBeUndefined();
      expect(daemon.activeTurnWaiterCount).toBe(0);
      // The fresh cursor sits at the current end, after the retained event.
      const afterReset = await client.sessionWaitTurn({
        session_ids: [session],
        until: ["turn_completed"],
        after: parkedReset.cursor,
        timeout_ms: SHORT_TURN_WAIT_MS,
      });
      expect(afterReset.reason).toBe("timeout");
      await client.close();
    } finally {
      await daemon.close();
      await other.close();
    }
  });

  test("invalid requests fail with the documented errors", async () => {
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("turn-invalid") },
      turnWait: { defaultTimeoutMs: LONG_TURN_WAIT_MS, maxTimeoutMs: TURN_WAIT_CEILING_MS },
    });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const session = await newShellSession(client);
      const invalid: unknown[] = [
        { session_ids: [], until: ["exited"] },
        { session_ids: [session, session], until: ["exited"] },
        { session_ids: [session], until: [] },
        { session_ids: [session], until: ["exited", "exited"] },
        { session_ids: [session], until: ["session_finished"] },
        { session_ids: [session], until: ["exited"], after: "" },
        { session_ids: [session], until: ["exited"], after: "has space" },
        { session_ids: [session], until: ["exited"], after: "x".repeat(257) },
        { session_ids: [session], until: ["exited"], heartbeat: true },
      ];
      for (const params of invalid) {
        const error = await expectClientError(client.sessionWaitTurn(params as SessionWaitTurnParams));
        expect(error.toProtocolError().code).toBe("bad_request");
      }
      for (const timeoutMs of [0, Number.NaN, Number.POSITIVE_INFINITY, 1.5, 4_294_967_296]) {
        const error = await expectClientError(client.sessionWaitTurn({
          session_ids: [session],
          until: ["exited"],
          timeout_ms: timeoutMs,
        }));
        expect(error.toProtocolError().code).toBe("bad_request");
      }
      const tooLong = await expectClientError(client.sessionWaitTurn({
        session_ids: ["s-testkit-missing"],
        until: ["exited"],
        timeout_ms: TURN_WAIT_CEILING_MS + 1,
      }));
      expect(tooLong.toProtocolError().code).toBe("session_wait_limit_exceeded");
      expect(daemon.activeTurnWaiterCount).toBe(0);
      // Refused requests leave the parent connection usable.
      expect((await client.call("session.inspect", session)).id).toBe(session);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("the client response deadline never cuts a turn wait short", async () => {
    const daemon = await startFixtureDaemon({
      listen: { unixSocketPath: testSocketPath("turn-deadline") },
      turnWait: { defaultTimeoutMs: LONG_TURN_WAIT_MS, maxTimeoutMs: TURN_WAIT_CEILING_MS },
    });
    try {
      // Setup and the usability check run on a client with the default deadline,
      // so only the turn waits depend on the short one.
      const setup = await connectLocal(requireUnixSocket(daemon));
      const session = await newShellSession(setup);
      const client = await connectLocal(requireUnixSocket(daemon), { requestTimeoutMs: SHORT_REQUEST_TIMEOUT_MS });
      const requested: SessionWaitTurnParams = {
        session_ids: [session],
        until: ["turn_completed"],
        timeout_ms: LONG_TURN_WAIT_MS,
      };
      const defaulted: SessionWaitTurnParams = { session_ids: [session], until: ["turn_completed"] };
      const results: SessionWaitTurnResult[] = [
        await client.sessionWaitTurn(requested),
        // An omitted timeout takes the daemon default, which the client cannot know.
        await client.sessionWaitTurn(defaulted),
        // Generic typed calls take the same dedicated path.
        await client.call("session.wait_turn", requested),
      ];
      for (const result of results) {
        expect(result.reason).toBe("timeout");
        expect(result.progress.map((entry) => entry.session_id)).toEqual([session]);
      }
      // The waits ran on dedicated connections and left this one usable.
      expect((await client.call("session.inspect", session)).id).toBe(session);
      await client.close();
      await setup.close();
    } finally {
      await daemon.close();
    }
  });

  test("fixture shutdown cancels active turn waiters", async () => {
    const daemon = await startFixtureDaemon({ listen: { unixSocketPath: testSocketPath("turn-shutdown") } });
    try {
      const client = await connectLocal(requireUnixSocket(daemon));
      const session = await newShellSession(client);
      const waiting = client.sessionWaitTurn({ session_ids: [session], until: ALL_TURN_KINDS })
        .catch((error: unknown) => error);
      await waitForTurnWaiters(daemon, 1);

      await daemon.close();

      expect(daemon.activeTurnWaiterCount).toBe(0);
      expect(await waiting).toBeInstanceOf(ClientError);
      await client.close();
    } finally {
      await daemon.close();
    }
  });
});

async function newShellSession(client: Client): Promise<string> {
  const created = await client.call("session.new", { agent: "shell", cols: TEST_COLS, rows: TEST_ROWS });
  return created.id;
}

async function waitForTurnWaiters(daemon: FixtureDaemonHandle, count: number): Promise<void> {
  await withTimeout(
    (async (): Promise<void> => {
      while (daemon.activeTurnWaiterCount < count) {
        await new Promise((resolve) => setTimeout(resolve, INPUT_WAITER_POLL_INTERVAL_MS));
      }
    })(),
    SOCKET_END_TIMEOUT_MS,
    "fixture turn waiter registration",
  );
}

function expectRfc3339(value: string | undefined): void {
  expect(typeof value).toBe("string");
  expect(Number.isNaN(Date.parse(value ?? ""))).toBe(false);
}

function eventOf(result: SessionWaitTurnResult): Record<string, unknown> | undefined {
  const event = result.event;
  if (event === undefined) {
    return undefined;
  }
  return { session_id: event.session_id, kind: event.kind, notification_id: event.notification_id };
}

async function expectProtocolError(promise: Promise<unknown>, code: string): Promise<void> {
  const error = await promise.catch((caught: unknown) => caught);
  expect(error).toBeInstanceOf(ClientError);
  expect((error as ClientError).toProtocolError().code).toBe(code);
}

// One private root per test file keeps every socket path short and out of shared directories.
const SOCKET_ROOT = createFixtureRootSync("pk-fx-");
process.on("exit", (): void => {
  rmSync(SOCKET_ROOT, { recursive: true, force: true });
});

function testSocketPath(label: string): string {
  return join(SOCKET_ROOT, `${label}.sock`);
}

function subscribeRequest(id: string): Request {
  return {
    v: CLIENT_PROTOCOL_VERSIONS,
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

/**
 * Sends `session.input` with a wait and reports activity once the fixture
 * holds the input waiter.
 *
 * The fixture counts only activity raised after the waiter opened, so the
 * report waits for the waiter instead of a timer; the timeout is a hang guard,
 * never the expected outcome.
 */
async function inputThenReport(
  daemon: FixtureDaemonHandle,
  client: Client,
  sessionId: string,
  wait: { readonly until?: Array<"idle"> },
  reported: readonly AgentActivity[] = ["idle"],
): Promise<SessionInputResult> {
  const waiting = client.call("session.input", {
    session_id: sessionId,
    text: "hello",
    wait: { ...wait, timeout_ms: MAX_SESSION_WAIT_MS },
  });
  await withTimeout(
    waitForActiveInputWaiter(daemon),
    SOCKET_END_TIMEOUT_MS,
    "fixture input waiter registration",
  );
  for (const activity of reported) {
    daemon.scenario.setAgentState(sessionId, activity, "report");
  }
  return waiting;
}

async function waitForActiveInputWaiter(daemon: FixtureDaemonHandle): Promise<void> {
  while (daemon.activeInputWaiterCount === 0) {
    await new Promise((resolve) => setTimeout(resolve, INPUT_WAITER_POLL_INTERVAL_MS));
  }
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
