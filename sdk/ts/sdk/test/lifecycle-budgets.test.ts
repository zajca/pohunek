import { describe, expect, jest, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { MAX_SESSION_WAIT_MS, PROTOCOL_VERSION } from "@pohunek/protocol";
import {
  Client,
  ClientError,
  DEFAULT_REQUEST_TIMEOUT_MS,
  LIFECYCLE_STEP_BOUND_MS,
  RESUME_SWEEP_PASSES,
  SESSION_FORK_REQUEST_BUDGET_MS,
  SESSION_NEW_REQUEST_BUDGET_MS,
  SESSION_REMOVE_REQUEST_BUDGET_MS,
  SESSION_RESUME_REQUEST_BUDGET_MS,
  SESSION_STOP_REQUEST_BUDGET_MS,
  WORKTREE_HOOK_TIMEOUT_MS,
  connectLocal,
  type ClientErrorKind,
  type ConnectOptions,
} from "@pohunek/sdk";
import {
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startMemoryDaemon,
  startUnixDaemon,
  type MockDaemon,
} from "./mock-daemon";

// The delayed reply outlasts an explicit deadline that still fits a direct
// round trip on a loaded runner (#494).
const LIFECYCLE_REPLY_DELAY_MS = 1_500;
const EXPLICIT_REQUEST_TIMEOUT_MS = 200;
const SCENARIO_TIMEOUT_MS = 10_000;

const fixturePath = join(dirname(fileURLToPath(import.meta.url)), "fixtures/lifecycle-budgets.json");

interface LifecycleCase {
  readonly method: "session.resume" | "session.fork" | "session.stop" | "session.remove" | "session.remove_accepting_unconfirmed";
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
    method: "session.fork",
    params: { session_id: "s-target", cwd_mode: "same", cols: 80, rows: 24 },
    result: minimalSessionInfo(),
    kind: "sessionForkOutcomeUnknown",
    code: "session_fork_outcome_unknown",
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
];

function healthLine(requestLine: string): string {
  return okResponseLine(requestIdFromLine(requestLine), {
    status: "ok",
    daemon_version: "test",
    protocol_version: PROTOCOL_VERSION,
  });
}

async function startLifecycleDaemon(result: unknown): Promise<MockDaemon> {
  return startUnixDaemon([
    { kind: "reply", line: healthLine },
    {
      kind: "delay",
      ms: LIFECYCLE_REPLY_DELAY_MS,
      line: (line) => okResponseLine(requestIdFromLine(line), result),
    },
  ]);
}

async function connect(daemon: MockDaemon, options?: ConnectOptions): Promise<Client> {
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport, options);
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
      session_fork_request_budget_ms: SESSION_FORK_REQUEST_BUDGET_MS,
      session_stop_request_budget_ms: SESSION_STOP_REQUEST_BUDGET_MS,
      worktree_hook_timeout_ms: WORKTREE_HOOK_TIMEOUT_MS,
      session_remove_request_budget_ms: SESSION_REMOVE_REQUEST_BUDGET_MS,
    }).toEqual(fixture);
    expect(SESSION_REMOVE_REQUEST_BUDGET_MS).toBe(
      SESSION_STOP_REQUEST_BUDGET_MS + 4 * WORKTREE_HOOK_TIMEOUT_MS,
    );
  });

  for (const lifecycle of LIFECYCLE_CASES) {
    test(`${lifecycle.method} outlasts the default request timeout within its budget`, async () => {
      let releaseReply: () => void = () => undefined;
      const ready = new Promise<void>((resolve) => {
        releaseReply = resolve;
      });
      const daemon = startMemoryDaemon([
        { kind: "gate", ready, line: (line) => okResponseLine(requestIdFromLine(line), lifecycle.result) },
        { kind: "reply", line: healthLine },
      ]);
      if (daemon.endpoint.kind !== "memory") {
        throw new Error("memory daemon returned a different transport");
      }
      // The client keeps its default request timeout; the mock reply waits past
      // it, on the virtual clock, but inside the lifecycle budget.
      jest.useFakeTimers();
      try {
        const client = await Client.connectTransport(daemon.endpoint.transport);
        const call = client.call(lifecycle.method, lifecycle.params as never);
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
      const daemon = await startLifecycleDaemon(lifecycle.result);
      try {
        const client = await connect(daemon, { requestTimeoutMs: EXPLICIT_REQUEST_TIMEOUT_MS });
        await client.call("daemon.health", null);

        const error = await expectClientError(client.call(lifecycle.method, lifecycle.params as never));
        expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe("daemon.health");
        expect(parseRequestLine(await daemon.nextRequest())["method"]).toBe(lifecycle.method);

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

  test("a retried fork is named as creating a second child in the recover hint", async () => {
    const daemon = await startLifecycleDaemon(minimalSessionInfo());
    try {
      const client = await connect(daemon, { requestTimeoutMs: EXPLICIT_REQUEST_TIMEOUT_MS });
      await client.call("daemon.health", null);
      const error = await expectClientError(
        client.call("session.fork", LIFECYCLE_CASES[1]!.params as never),
      );
      expect(error.recoverHint()).toContain("second child");
      await client.close();
    } finally {
      await daemon.close();
    }
  }, SCENARIO_TIMEOUT_MS);

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
