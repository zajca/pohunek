import { describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type ProtocolVersionRange } from "@pohunek/protocol";
import {
  CLIENT_PROTOCOL_VERSIONS,
  ClientError,
  connectTcp,
  nextRequestId,
  type SessionListFilter,
  type SessionWaitParams,
} from "@pohunek/sdk";
import {
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
  type ScriptStep,
} from "./mock-daemon";

/**
 * A request that waits for a version probe must never be written after the
 * caller gave up, and a caller always keeps the parameters it invoked with.
 * Every scenario drives the production client against a real TCP daemon whose
 * reply order and latency the test scripts, so the probe answer arrives at a
 * point the test chooses and the daemon records exactly what was written.
 */

const HOST = "netbird:late-negotiation";
const UNINSTALL = { digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000" };

// The daemon holds its probe reply until the caller times out or closes.
// A bounded negative assertion then catches a late write on the real socket.
const PROBE_TIMEOUT_MS = 200;
const SETTLE_MS = 200;

function healthReply(): (requestLine: string) => string {
  return (requestLine) => okResponseLine(requestIdFromLine(requestLine), {
    status: "ok",
    daemon_version: "0.0.0-test",
    protocol_version: PROTOCOL_VERSION,
  });
}

function healthGate(): { step: ScriptStep; release: () => void } {
  let release = (): void => {};
  const ready = new Promise<void>((resolve) => {
    release = (): void => resolve();
  });
  return { step: { kind: "gate", ready, line: healthReply() }, release };
}

function exact(version: number): ProtocolVersionRange {
  return { minimum: version, maximum: version };
}

function minimalWaitSession(): Record<string, unknown> {
  return {
    id: "s-1",
    capabilities: { resume: true, fork: true },
    agent: "codex",
    agent_base: "codex",
    cwd: "/workspace/pohunek",
    pid: 42,
    cols: 80,
    rows: 24,
    state: "running",
    state_source: "process",
    created_at: "2026-07-08T00:00:00Z",
    updated_at: "2026-07-08T00:01:00Z",
  };
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

function address(daemon: MockDaemon): { host: string; port: number } {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("late negotiation tests require a real TCP endpoint");
  }
  return { host: daemon.endpoint.host, port: daemon.endpoint.port };
}

function uninstallRequest(): {
  v: typeof CLIENT_PROTOCOL_VERSIONS;
  id: string;
  method: "package.uninstall";
  params: { digest: string };
} {
  return {
    v: CLIENT_PROTOCOL_VERSIONS,
    id: nextRequestId("package.uninstall"),
    method: "package.uninstall",
    params: UNINSTALL,
  };
}

describe("late version negotiation over a real TCP daemon", () => {
  test("a request is not written when the probe answers after the caller timed out", async () => {
    const gate = healthGate();
    const daemon = await startTcpDaemon([
      gate.step,
      // The answer a correct client never asks for: `package.uninstall` was
      // abandoned with the probe, so the daemon must never see this step.
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { removed: true }) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon), { requestTimeoutMs: PROBE_TIMEOUT_MS });
      const outcome = client.request(uninstallRequest());
      const probe = parseRequestLine(await daemon.nextRequest());
      expect(probe["method"]).toBe("daemon.health");
      const error = await expectClientError(outcome);
      expect(error.kind).toBe("requestTimeout");

      gate.release();
      await daemon.expectNoRequest(SETTLE_MS);

      // The connection is poisoned; further calls fail locally and stay silent.
      const retry = await expectClientError(client.call("daemon.health", null));
      expect(retry.kind).toBe("framing");
      await daemon.expectNoRequest(SETTLE_MS);
      await client.close();
    } finally {
      gate.release();
      await daemon.close();
    }
  });

  test("a request is not written when the client was closed while the probe was in flight", async () => {
    const gate = healthGate();
    const daemon = await startTcpDaemon([
      gate.step,
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { removed: true }) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon), { requestTimeoutMs: 5_000 });
      const outcome = client.request(uninstallRequest()).then(
        () => undefined,
        (error: unknown) => error,
      );

      const probe = parseRequestLine(await daemon.nextRequest());
      expect(probe["method"]).toBe("daemon.health");
      await client.close();
      gate.release();
      await outcome;
      await daemon.expectNoRequest(SETTLE_MS);
    } finally {
      gate.release();
      await daemon.close();
    }
  });
});

describe("caller-owned parameters over a real TCP daemon", () => {
  test("a request keeps the parameters it was invoked with", async () => {
    const daemon = await startTcpDaemon([
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), []) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon), { requestTimeoutMs: 5_000 });
      const params = {
        filters: [{ key: "state", value: "running" } as SessionListFilter],
        note: "original",
      };
      const outcome = client.call("session.list", params);
      // The caller may change its object once the call has returned a promise;
      // what reaches the daemon is the snapshot from the invocation.
      params.note = "changed";
      params.filters[0]!.value = "changed";

      expect(await outcome).toEqual([]);
      const sent = parseRequestLine(await daemon.nextRequest());
      expect(sent["method"]).toBe("session.list");
      expect(sent["params"]).toEqual({
        filters: [{ key: "state", value: "running" }],
        note: "original",
      });
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("a request keeps its parameters while the version probe is pending", async () => {
    const gate = healthGate();
    const daemon = await startTcpDaemon([
      gate.step,
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { removed: true }) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon), { requestTimeoutMs: 5_000 });
      const params = { digest: UNINSTALL.digest };
      const request = { ...uninstallRequest(), params };
      // Mutated while the probe exchange is still pending.
      const outcome = client.request(request);
      const probe = parseRequestLine(await daemon.nextRequest());
      expect(probe["method"]).toBe("daemon.health");
      params.digest = "sha256:changed";
      gate.release();

      expect(await outcome).toEqual({ removed: true });
      const sent = parseRequestLine(await daemon.nextRequest());
      expect(sent["method"]).toBe("package.uninstall");
      expect((sent["params"] as { digest: string }).digest).toBe(UNINSTALL.digest);
      // A request whose version already settled is pinned to that version.
      expect(sent["v"]).toEqual(exact(PROTOCOL_VERSION));
      await client.close();
    } finally {
      gate.release();
      await daemon.close();
    }
  });

  test("a dedicated call keeps the parameters it was invoked with", async () => {
    // The runtime evidence makes `session.wait` version-dependent, so the
    // dedicated connection must learn the version on its own dial before it
    // writes the call.
    const gate = healthGate();
    const daemon = await startTcpDaemon([
      gate.step,
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), {
        reason: "timeout",
        session: minimalWaitSession(),
        terminal_watermark: "0",
        output_offset: "0",
      }) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon), { requestTimeoutMs: 5_000 });
      const params: SessionWaitParams = {
        session_id: "s-1",
        runtime: { worker_instance_id: "runtime-target", runtime_generation: "1" },
        timeout_ms: 25,
      };
      const outcome = client.sessionWait(params);
      const probe = parseRequestLine(await daemon.nextRequest());
      expect(probe["method"]).toBe("daemon.health");
      (params as { session_id: string }).session_id = "s-changed";
      gate.release();

      expect(await outcome).toEqual({
        reason: "timeout",
        session: minimalWaitSession(),
        terminal_watermark: "0",
        output_offset: "0",
      });
      const sent = parseRequestLine(await daemon.nextRequest());
      expect(sent["method"]).toBe("session.wait");
      expect((sent["params"] as { session_id: string }).session_id).toBe("s-1");
      expect((sent["params"] as { runtime?: { worker_instance_id: string } }).runtime?.worker_instance_id)
        .toBe("runtime-target");
      expect(sent["v"]).toEqual(exact(PROTOCOL_VERSION));
      await client.close();
    } finally {
      gate.release();
      await daemon.close();
    }
  });
});
