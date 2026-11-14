import { describe, expect, jest, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { MAX_SESSION_REMOVE_TREE_SESSIONS, MAX_SESSION_WAIT_MS, PROTOCOL_VERSION } from "@pohunek/protocol";
import {
  Client,
  ClientError,
  DEFAULT_REQUEST_TIMEOUT_MS,
  LIFECYCLE_STEP_BOUND_MS,
  RESUME_SWEEP_PASSES,
  SESSION_NEW_REQUEST_BUDGET_MS,
  SESSION_REMOVE_REQUEST_BUDGET_MS,
  SESSION_REMOVE_TREE_REQUEST_BUDGET_MS,
  SESSION_RESUME_REQUEST_BUDGET_MS,
  SESSION_STOP_REQUEST_BUDGET_MS,
  WORKTREE_HOOK_TIMEOUT_MS,
  connectLocal,
  connectTcp,
  type ClientErrorKind,
  type ConnectOptions,
} from "@pohunek/sdk";
import {
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startMemoryDaemon,
  startTcpDaemon,
  startUnixDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

// The delayed reply outlasts an explicit deadline that still fits a direct
// round trip on a loaded runner (#494).
const LIFECYCLE_REPLY_DELAY_MS = 1_500;
const EXPLICIT_REQUEST_TIMEOUT_MS = 200;
const SCENARIO_TIMEOUT_MS = 10_000;

const fixturePath = join(dirname(fileURLToPath(import.meta.url)), "fixtures/lifecycle-budgets.json");

interface LifecycleCase {
  readonly method:
    | "session.resume"
    | "session.stop"
    | "session.remove"
    | "session.remove_accepting_unconfirmed"
    | "session.remove_tree";
  readonly params: unknown;
  readonly result: unknown;
  readonly kind: ClientErrorKind;
  readonly code: string;
}

const LIFECYCLE_CASES: readonly LifecycleCase[] = [
  {
    method: "session.resume",
    params: { session_id: "s-target", accept_profile_change: false },
    result: { session: minimalSessionInfo() },
    kind: "sessionResumeOutcomeUnknown",
    code: "session_resume_outcome_unknown",
  },
  {
    method: "session.stop",
    params: "s-target",
    result: { stopped: true },
    kind: "sessionStopOutcomeUnknown",
    code: "session_stop_outcome_unknown",
  },
  {
    method: "session.remove",
    params: "s-target",
    result: { removed: true, stopped: true, worktrees_removed: 0, worktrees_failed: 0 },
    kind: "sessionRemoveOutcomeUnknown",
    code: "session_remove_outcome_unknown",
  },
  {
    method: "session.remove_accepting_unconfirmed",
    params: "s-target",
    result: { removed: true, stopped: true, worktrees_removed: 0, worktrees_failed: 0 },
    kind: "sessionRemoveOutcomeUnknown",
    code: "session_remove_outcome_unknown",
  },
  {
    method: "session.remove_tree",
    params: { session_id: "s-target" },
    result: {
      removed: 1,
      kept: 0,
      refused: 0,
      sessions: [{ session_id: "s-target", depth: 0, outcome: "removed", keep_reason: null }],
    },
    kind: "sessionRemoveTreeOutcomeUnknown",
    code: "session_remove_tree_outcome_unknown",
  },
];

function inspectStep(flags: Record<string, unknown>): ScriptStep {
  // The probe answer keeps the channel open: the dedicated connection sends
  // the tree request on this same channel right after the probe.
  return {
    kind: "gate",
    ready: Promise.resolve(),
    line: (line) =>
      okResponseLine(requestIdFromLine(line), {
        daemon_version: "0.0.0-test",
        protocol_version: PROTOCOL_VERSION,
        supported_agents: [],
        runtimes: [],
        git_available: true,
        ...flags,
      }),
  };
}

function healthLine(requestLine: string): string {
  return okResponseLine(requestIdFromLine(requestLine), {
    status: "ok",
    daemon_version: "test",
    protocol_version: PROTOCOL_VERSION,
  });
}

function inspectFlagsFor(method: LifecycleCase["method"]): Record<string, unknown> {
  if (method === "session.remove_tree") {
    return { session_remove_tree_supported: true };
  }
  return {};
}

async function startLifecycleDaemon(
  lifecycle: LifecycleCase,
  result: unknown,
): Promise<MockDaemon> {
  // `session.remove_tree` probes `host.inspect` for its capability flag before
  // the dedicated connection sends the request, so its scenario answers the
  // probe first and only then delays the tree reply.
  const inspect = inspectStep(inspectFlagsFor(lifecycle.method));
  const delayed = {
    kind: "delay",
    ms: LIFECYCLE_REPLY_DELAY_MS,
    line: (line: string) => okResponseLine(requestIdFromLine(line), result),
  } as const;
  if (lifecycle.method === "session.remove_tree") {
    return startTcpDaemon([inspect, delayed]);
  }
  return startUnixDaemon([{ kind: "reply", line: healthLine }, delayed]);
}

async function connect(daemon: MockDaemon, options?: ConnectOptions): Promise<Client> {
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport, options);
  }
  if (daemon.endpoint.kind === "tcp") {
    return connectTcp("lifecycle-budgets", { host: daemon.endpoint.host, port: daemon.endpoint.port }, options);
  }
  if (daemon.endpoint.kind !== "unix") {
    throw new Error("lifecycle scenarios use the Unix or in-memory daemon");
  }
  return connectLocal(daemon.endpoint.socketPath, options);
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

describe("lifecycle response budgets", () => {
  test("the budgets equal the Rust client's, taken from the shared fixture", () => {
    const fixture = JSON.parse(readFileSync(fixturePath, "utf8")) as Record<string, number>;
    expect({
      lifecycle_step_bound_ms: LIFECYCLE_STEP_BOUND_MS,
      resume_sweep_passes: RESUME_SWEEP_PASSES,
      session_new_request_budget_ms: SESSION_NEW_REQUEST_BUDGET_MS,
      session_resume_request_budget_ms: SESSION_RESUME_REQUEST_BUDGET_MS,
      session_stop_request_budget_ms: SESSION_STOP_REQUEST_BUDGET_MS,
      worktree_hook_timeout_ms: WORKTREE_HOOK_TIMEOUT_MS,
      session_remove_request_budget_ms: SESSION_REMOVE_REQUEST_BUDGET_MS,
      session_remove_tree_request_budget_ms: SESSION_REMOVE_TREE_REQUEST_BUDGET_MS,
    }).toEqual(fixture);
    expect(SESSION_REMOVE_REQUEST_BUDGET_MS).toBe(
      SESSION_STOP_REQUEST_BUDGET_MS + 4 * WORKTREE_HOOK_TIMEOUT_MS,
    );
    expect(SESSION_REMOVE_TREE_REQUEST_BUDGET_MS).toBe(
      MAX_SESSION_REMOVE_TREE_SESSIONS * SESSION_REMOVE_REQUEST_BUDGET_MS,
    );
  });

  for (const lifecycle of LIFECYCLE_CASES) {
    test(`${lifecycle.method} outlasts the default request timeout within its budget`, async () => {
      let releaseReply: () => void = () => undefined;
      const ready = new Promise<void>((resolve) => {
        releaseReply = resolve;
      });
      // `session.remove_tree` probes `host.inspect` first, so its gate waits
      // behind the capability reply.
      const steps: ScriptStep[] =
        lifecycle.method === "session.remove_tree"
          ? [
            inspectStep(inspectFlagsFor(lifecycle.method)),
            {
              kind: "gate",
              ready,
              line: (line) => okResponseLine(requestIdFromLine(line), lifecycle.result),
            },
            { kind: "reply", line: healthLine },
          ]
          : [
            {
              kind: "gate",
              ready,
              line: (line) => okResponseLine(requestIdFromLine(line), lifecycle.result),
            },
            { kind: "reply", line: healthLine },
          ];
      const daemon = startMemoryDaemon(steps);
      if (daemon.endpoint.kind !== "memory") {
        throw new Error("memory daemon returned a different transport");
      }
      // The client keeps its default request timeout; the mock reply waits past
      // it, on the virtual clock, but inside the lifecycle budget.
      jest.useFakeTimers();
      try {
        const client = await Client.connectTransport(daemon.endpoint.transport);
        const call = client.call(lifecycle.method, lifecycle.params as never);
        // The dedicated connection writes its first request before the test
        // starts draining; a flush hands the turn to the mock's gate reply.
        await Promise.resolve();
        await Promise.resolve();
        if (lifecycle.method === "session.remove_tree") {
          expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("host.inspect");
        }
        expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe(lifecycle.method);
        jest.advanceTimersByTime(DEFAULT_REQUEST_TIMEOUT_MS + 1_000);
        releaseReply();

        expect(await call).toEqual(lifecycle.result);
        // The shared connection stayed usable.
        expect(await client.call("daemon.health", null)).toEqual({
          status: "ok",
          daemon_version: "test",
          protocol_version: PROTOCOL_VERSION,
        });
        await client.close();
      } finally {
        jest.useRealTimers();
        await daemon.close();
      }
    });

    test(`an explicit timeout wins over the ${lifecycle.method} budget and reports an unknown outcome`, async () => {
      const daemon = await startLifecycleDaemon(lifecycle, lifecycle.result);
      try {
        const client = await connect(daemon, { requestTimeoutMs: EXPLICIT_REQUEST_TIMEOUT_MS });
        await client.call("daemon.health", null);

        const error = await expectClientError(client.call(lifecycle.method, lifecycle.params as never));
        expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
        if (lifecycle.method === "session.remove_tree") {
          // The capability probe itself outlasts the explicit deadline, so the
          // tree request never reaches the wire: the unknown outcome already
          // covers the cascade.
          expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("host.inspect");
        } else {
          expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe(lifecycle.method);
        }

        expect(error.kind).toBe(lifecycle.kind);
        expect(error.errorClass).toBe("transport");
        expect(error.code).toBe(lifecycle.code);
        expect(error.recoverHint()).toContain("may still complete");
        await client.close();
      } finally {
        await daemon.close();
      }
    }, SCENARIO_TIMEOUT_MS);
  }

  test("session.new with the largest input_ack_timeout_ms outlasts the creation budget", async () => {
    const created = {
      id: "s-ack",
      agent: "shell",
      agent_base: "shell",
      capabilities: { fork: false, resume: false },
      cwd: "/project",
      pid: 42,
      state: "running",
      state_source: "process",
      cols: 80,
      rows: 24,
      created_at: "2026-07-08T00:00:00Z",
      updated_at: "2026-07-08T00:01:00Z",
      applied_input: true,
      prompt_delivered: false,
    };
    let releaseReply: () => void = () => undefined;
    const ready = new Promise<void>((resolve) => {
      releaseReply = resolve;
    });
    const daemon = startMemoryDaemon([
      { kind: "reply", line: healthLine },
      { kind: "gate", ready, line: (line) => okResponseLine(requestIdFromLine(line), created) },
    ]);
    if (daemon.endpoint.kind !== "memory") {
      throw new Error("memory daemon returned a different transport");
    }
    jest.useFakeTimers();
    try {
      const client = await Client.connectTransport(daemon.endpoint.transport);
      // The acknowledgement window makes the request version-dependent, so the
      // connection learns the daemon's version before the creation is sent.
      await client.call("daemon.health", null);
      const call = client.call("session.new", {
        agent: "shell",
        cols: 80,
        rows: 24,
        input: "hello",
        input_ack_timeout_ms: MAX_SESSION_WAIT_MS,
      });
      expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
      expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("session.new");
      // Past the creation budget, inside the budget plus the delivery bound and
      // the acknowledgement window.
      jest.advanceTimersByTime(SESSION_NEW_REQUEST_BUDGET_MS + 2 * MAX_SESSION_WAIT_MS - 1_000);
      releaseReply();

      expect(await call).toEqual(created);
      await client.close();
    } finally {
      jest.useRealTimers();
      await daemon.close();
    }
  });
});
