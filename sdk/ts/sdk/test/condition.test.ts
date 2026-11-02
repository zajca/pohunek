import { afterAll, describe, expect, test } from "bun:test";
import type { RuntimeState, SessionInfo, SessionRuntime } from "@pohunek/protocol";
import { connectLocal, sessionCondition, type Client, type SessionConditionResult } from "@pohunek/sdk";
import { createFixtureRootSync, startFixtureDaemon, type FixtureDaemonHandle } from "@pohunek/testkit";
import { rmSync } from "node:fs";
import { minimalSessionInfo } from "./mock-daemon";

/**
 * `sessionCondition` against the sessions the fixture daemon serves through
 * `session.list`: one row per rule and per recovery kind.
 */

function runtimeOf(state: string, overrides: Partial<SessionRuntime> = {}): SessionRuntime {
  // The generated union is the daemon's contract; a value outside it is what a newer daemon sends.
  return { state: state as RuntimeState, runtime_generation: "1", ...overrides };
}

function sessionWith(id: string, overrides: Partial<SessionInfo>): SessionInfo {
  return { ...minimalSessionInfo(), id, ...overrides };
}

const LOST: SessionRuntime = runtimeOf("lost", { loss_reason: "worker_lost" });

const SEEDED = {
  liveRunning: sessionWith("s-cond-live", { runtime: runtimeOf("live") }),
  noRuntimeStarting: sessionWith("s-cond-starting", { state: "starting" }),
  externalLost: sessionWith("s-cond-external", { external: true, runtime: LOST }),
  failedLost: sessionWith("s-cond-failed-lost", {
    state: "failed",
    runtime: LOST,
    capabilities: { resume: true, fork: true },
  }),
  runningLost: sessionWith("s-cond-running-lost", { runtime: LOST, capabilities: { resume: true, fork: true } }),
  failedTerminal: sessionWith("s-cond-failed-terminal", {
    state: "failed",
    runtime: runtimeOf("terminal"),
  }),
  stoppedNoRuntime: sessionWith("s-cond-stopped", { state: "stopped" }),
  runningConflict: sessionWith("s-cond-conflict", { runtime: runtimeOf("conflict") }),
  startingReconnecting: sessionWith("s-cond-reconnecting", {
    state: "starting",
    runtime: runtimeOf("reconnecting"),
  }),
  runningIncompatible: sessionWith("s-cond-incompatible", { runtime: runtimeOf("incompatible") }),
  runningUnknownRuntime: sessionWith("s-cond-unknown-runtime", { runtime: runtimeOf("draining") }),
  stoppedLost: sessionWith("s-cond-stopped-lost", { state: "stopped", runtime: LOST }),
  doneUnknownRuntime: sessionWith("s-cond-done-unknown-runtime", { state: "done", runtime: runtimeOf("draining") }),
  lostNoNativeResume: sessionWith("s-cond-no-resume", {
    state: "failed",
    runtime: LOST,
    capabilities: { resume: false, fork: false },
  }),
  lostResumeRefused: sessionWith("s-cond-refused", {
    state: "failed",
    runtime: LOST,
    capabilities: { resume: true, fork: true },
    capability_refusals: { resume: "agent_resume_unsupported" },
  }),
} satisfies Record<string, SessionInfo>;

const root = createFixtureRootSync("pk-cond-");
process.on("exit", (): void => {
  rmSync(root, { recursive: true, force: true });
});

let daemon: FixtureDaemonHandle | undefined;
let client: Client | undefined;
let listed: Map<string, SessionInfo> | undefined;

async function listedSessions(): Promise<Map<string, SessionInfo>> {
  if (listed !== undefined) {
    return listed;
  }
  daemon = await startFixtureDaemon({
    listen: { unixSocketPath: `${root}/condition.sock` },
    initialSessions: Object.values(SEEDED),
  });
  client = await connectLocal(`${root}/condition.sock`);
  const sessions = await client.call("session.list", {});
  listed = new Map(sessions.map((session) => [session.id, session]));
  return listed;
}

async function listedCondition(id: string, options?: { listed?: boolean }): Promise<SessionConditionResult> {
  const sessions = await listedSessions();
  const info = sessions.get(id);
  if (info === undefined) {
    throw new Error(`fixture daemon did not list ${id}`);
  }
  return sessionCondition(info, options);
}

afterAll(async () => {
  await client?.close();
  await daemon?.close();
});

describe("sessionCondition against the fixture daemon session.list", () => {
  test("rule 6: a live runtime with a running session reads as the state", async () => {
    expect(await listedCondition("s-cond-live")).toEqual({ condition: "running", recovery: null });
  });

  test("rule 6: a starting session without a runtime reads as the state", async () => {
    expect(await listedCondition("s-cond-starting")).toEqual({ condition: "starting", recovery: null });
  });

  test("rule 1: an external session keeps its state label even with a lost runtime", async () => {
    expect(await listedCondition("s-cond-external")).toEqual({ condition: "running", recovery: null });
  });

  test("rule 2: a failed session with a lost runtime is crashed", async () => {
    expect(await listedCondition("s-cond-failed-lost", { listed: true })).toEqual({
      condition: "crashed",
      recovery: { kind: "recoverable" },
    });
  });

  test("rule 2: an unproven loss on a running session is crashed", async () => {
    expect(await listedCondition("s-cond-running-lost", { listed: true })).toEqual({
      condition: "crashed",
      recovery: { kind: "recoverable" },
    });
  });

  test("rule 2: a lost runtime is crashed even when the session is stopped", async () => {
    expect((await listedCondition("s-cond-stopped-lost")).condition).toBe("crashed");
  });

  test("rule 3: a failed session whose runtime is terminal stays failed", async () => {
    expect(await listedCondition("s-cond-failed-terminal")).toEqual({ condition: "failed", recovery: null });
  });

  test("rule 3: a stopped session without a runtime reads as stopped", async () => {
    expect(await listedCondition("s-cond-stopped")).toEqual({ condition: "stopped", recovery: null });
  });

  test("rule 4: a running session with a conflicting runtime reads as conflict", async () => {
    expect(await listedCondition("s-cond-conflict")).toEqual({ condition: "conflict", recovery: null });
  });

  test("rule 4: a starting session with a reconnecting runtime reads as reconnecting", async () => {
    expect(await listedCondition("s-cond-reconnecting")).toEqual({ condition: "reconnecting", recovery: null });
  });

  test("rule 4: a running session with an incompatible runtime reads as incompatible", async () => {
    expect(await listedCondition("s-cond-incompatible")).toEqual({ condition: "incompatible", recovery: null });
  });

  test("rule 5: a runtime state this build does not know reads as unknown, never crashed", async () => {
    expect(await listedCondition("s-cond-unknown-runtime")).toEqual({ condition: "unknown", recovery: null });
  });

  test("rule 5 does not apply to a session that is not live", async () => {
    expect(await listedCondition("s-cond-done-unknown-runtime")).toEqual({ condition: "done", recovery: null });
  });

  test("recovery no_native_resume: a lost session whose agent cannot resume", async () => {
    expect(await listedCondition("s-cond-no-resume", { listed: true })).toEqual({
      condition: "crashed",
      recovery: { kind: "no_native_resume" },
    });
  });

  test("recovery refused: a lost session with a resume refusal carries its code", async () => {
    expect(await listedCondition("s-cond-refused", { listed: true })).toEqual({
      condition: "crashed",
      recovery: { kind: "refused", code: "agent_resume_unsupported" },
    });
  });

  test("recovery unknown: a lost session that did not come from session.list or session.inspect", async () => {
    expect(await listedCondition("s-cond-failed-lost")).toEqual({
      condition: "crashed",
      recovery: { kind: "unknown" },
    });
  });
});
