import { describe, expect, test } from "bun:test";
import { MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS, type ProtocolError, type SessionInfo } from "@pohunek/protocol";
import {
  Client,
  ClientError,
  connectLocal,
  connectTcp,
  isRequest,
  type ConnectOptions,
  type Request,
} from "@pohunek/sdk";
import {
  errResponseLine,
  minimalSessionInfo,
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  startUnixDaemon,
  type MockDaemon,
} from "./mock-daemon";

describe("Client request/response", () => {
  test("request decoder accepts ordered ranges and rejects legacy exact versions", () => {
    expect(isRequest({
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: "range-request",
      method: "daemon.health",
      params: null,
    })).toBe(true);
    expect(isRequest({
      v: PROTOCOL_VERSION,
      id: "legacy-request",
      method: "daemon.health",
      params: null,
    })).toBe(false);
    expect(isRequest({
      v: { minimum: 2, maximum: 1 },
      id: "reversed-request",
      method: "daemon.health",
      params: null,
    })).toBe(false);
    expect(isRequest({
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: "origin-request",
      method: "daemon.health",
      params: null,
      origin_session_id: "s-origin",
      origin_daemon_id: "daemon-origin",
    })).toBe(true);
    expect(isRequest({
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: "partial-origin",
      method: "daemon.health",
      params: null,
      origin_session_id: "s-origin",
    })).toBe(false);
    expect(isRequest({
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: "unsafe-origin",
      method: "daemon.health",
      params: null,
      origin_session_id: "s-origin",
      origin_daemon_id: "daemon origin",
    })).toBe(false);
    expect(isRequest({
      v: SUPPORTED_PROTOCOL_VERSIONS,
      id: "oversized-origin",
      method: "daemon.health",
      params: null,
      origin_session_id: "s-origin",
      origin_daemon_id: "d".repeat(129),
    })).toBe(false);
  });

  test("call decodes a typed session.list output and sends method params", async () => {
    const session = minimalSessionInfo();
    const daemon = await startUnixDaemon([
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), [session]) },
    ]);
    try {
      const client = await connectClient(daemon);

      const result = await client.call("session.list", {
        filters: [{ key: "state", value: "running" }],
      });

      expect(result).toEqual([session] satisfies SessionInfo[]);
      const sent = parseRequestLine(await daemon.nextRequest());
      expect(sent["v"]).toEqual(SUPPORTED_PROTOCOL_VERSIONS);
      expect(sent["method"]).toBe("session.list");
      expect(sent["params"]).toEqual({ filters: [{ key: "state", value: "running" }] });
      expect(sent["origin_session_id"]).toBeUndefined();
      expect(sent["origin_daemon_id"]).toBeUndefined();
      expect(String(sent["id"])).toStartWith("sdk-session.list-");
    } finally {
      await daemon.close();
    }
  });

  test("err response rejects with a passthrough protocol error", async () => {
    const source: ProtocolError = {
      class: "runtime",
      code: "agent_failed",
      msg: "agent failed during test",
      recover: "retry the request",
    };
    const daemon = await startUnixDaemon([
      { kind: "reply", line: (requestLine) => errResponseLine(requestIdFromLine(requestLine), source) },
    ]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.call("daemon.health", null));

      const structured = error.toProtocolError();
      expect(structured.class).toBe(source.class);
      expect(structured.code).toBe(source.code);
      expect(structured.recover).toBe(source.recover);
    } finally {
      await daemon.close();
    }
  });

  test("remote err response preserves protocol class and code while adding host context", async () => {
    const source: ProtocolError = {
      class: "runtime",
      code: "agent_failed",
      msg: "agent failed during test",
      recover: "retry the request",
    };
    const daemon = await startTcpDaemon([
      { kind: "reply", line: (requestLine) => errResponseLine(requestIdFromLine(requestLine), source) },
    ]);
    try {
      const client = await connectClient(daemon, "build-box");

      const error = await expectClientError(client.call("daemon.health", null));

      const structured = error.toProtocolError();
      expect(structured.class).toBe(source.class);
      expect(structured.code).toBe(source.code);
      expect(structured.recover).toBe(source.recover);
      expect(structured.msg).toContain("build-box");
    } finally {
      await daemon.close();
    }
  });

  test("garbled JSON reply maps to json_error and poisons the connection", async () => {
    const daemon = await startUnixDaemon([
      { kind: "garbled" },
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { status: "ok" }) },
    ]);
    try {
      const client = await connectClient(daemon);

      const firstError = await expectClientError(client.call("daemon.health", null));
      expect(firstError.toProtocolError().class).toBe("daemon");
      expect(firstError.toProtocolError().code).toBe("json_error");
      await daemon.nextRequest();

      const secondError = await expectClientError(client.call("daemon.health", null));
      expect(secondError.toProtocolError().class).toBe("transport");
      expect(secondError.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
    } finally {
      await daemon.close();
    }
  });

  test("daemon close without a response maps to the no-response framing error", async () => {
    const daemon = await startUnixDaemon([{ kind: "close" }]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().class).toBe("transport");
      expect(error.toProtocolError().code).toBe("framing");
    } finally {
      await daemon.close();
    }
  });

  test("oversized reply maps to a framing error", async () => {
    const daemon = await startUnixDaemon([{ kind: "oversized" }]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().class).toBe("transport");
      expect(error.toProtocolError().code).toBe("framing");
    } finally {
      await daemon.close();
    }
  });

  test("oversized request is rejected before the daemon receives it", async () => {
    const daemon = await startUnixDaemon([{ kind: "silent" }]);
    try {
      const client = await connectClient(daemon);
      const request: Request = {
        v: SUPPORTED_PROTOCOL_VERSIONS,
        id: "req-too-large",
        method: "daemon.health",
        params: { payload: "x".repeat(MAX_CONTROL_LINE_BYTES + 1) },
      };

      const error = await expectClientError(client.request(request));

      expect(error.toProtocolError().class).toBe("transport");
      expect(error.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
    } finally {
      await daemon.close();
    }
  });

  test("handshake rejects a protocol version mismatch", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) =>
          okResponseLine(requestIdFromLine(requestLine), {
            status: "ok",
            daemon_version: "0.15.1",
            protocol_version: PROTOCOL_VERSION + 1,
          }),
      },
    ]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.handshake());

      expect(error.toProtocolError().class).toBe("daemon");
      expect(error.toProtocolError().code).toBe("version_mismatch");
    } finally {
      await daemon.close();
    }
  });

  test("response outside the offered range is rejected and poisons the connection", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) => JSON.stringify({
          v: PROTOCOL_VERSION + 1,
          id: requestIdFromLine(requestLine),
          ok: { status: "ok" },
        }),
      },
    ]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().code).toBe("version_mismatch");
      const poisoned = await expectClientError(client.call("daemon.health", null));
      expect(poisoned.toProtocolError().code).toBe("framing");
    } finally {
      await daemon.close();
    }
  });

  test("typed screen, output, and wait methods preserve observation payloads", async () => {
    const screen = {
      session_id: "s-test-1",
      worker_id: "worker-test-1",
      runtime_id: "runtime-test-1",
      runtime_generation: "1",
      watermark: "2",
      dimensions: { cols: 80, rows: 24 },
      cursor: { row: 0, col: 3, visible: true },
      alternate_screen: false,
      visible_lines: ["test"],
    } as const;
    const output = {
      session_id: "s-test-1",
      runtime_id: "runtime-test-1",
      runtime_generation: "1",
      history_start_offset: "0",
      start_offset: "0",
      next_offset: "4",
      runtime_end_offset: "4",
      data_base64: "dGVzdA==",
      has_more: false,
      timed_out: false,
    } as const;
    const wait = {
      reason: "output_advanced",
      session: minimalSessionInfo(),
      terminal_watermark: "2",
      output_offset: "4",
    } as const;
    const daemon = await startUnixDaemon([
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), screen) },
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), output) },
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), wait) },
    ]);
    try {
      const screenClient = await connectClient(daemon);
      expect(await screenClient.sessionScreen({ session_id: "s-test-1" })).toEqual(screen);
      await screenClient.close();
      const outputClient = await connectClient(daemon);
      expect(await outputClient.sessionOutput({ session_id: "s-test-1", max_bytes: 128 })).toEqual(output);
      await outputClient.close();
      const waitClient = await connectClient(daemon);
      expect(await waitClient.sessionWait({
        session_id: "s-test-1",
        after_updated_at: "2026-07-08T00:00:00Z",
        timeout_ms: 50,
      })).toEqual(wait);
      await waitClient.close();
    } finally {
      await daemon.close();
    }
  });

  test("configured origin reaches ordinary and dedicated observation connections", async () => {
    const screen = {
      session_id: "s-target",
      worker_id: "worker-target",
      runtime_id: "runtime-target",
      runtime_generation: "1",
      watermark: "2",
      dimensions: { cols: 80, rows: 24 },
      cursor: { row: 0, col: 0, visible: true },
      alternate_screen: false,
      visible_lines: [],
    } as const;
    const output = {
      session_id: "s-target",
      runtime_id: "runtime-target",
      runtime_generation: "1",
      history_start_offset: "0",
      start_offset: "0",
      next_offset: "0",
      runtime_end_offset: "0",
      data_base64: "",
      has_more: false,
      timed_out: true,
    } as const;
    const wait = {
      reason: "timeout",
      session: minimalSessionInfo(),
      terminal_watermark: "2",
      output_offset: "0",
    } as const;
    const input = {
      accepted: true,
      activity: "idle",
      activity_source: "report",
      runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
      activity_epoch: "d-epoch-1",
      activity_revision: "2",
    } as const;
    const daemon = await startUnixDaemon([
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), screen) },
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), input) },
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), output) },
      { kind: "reply", line: (line) => okResponseLine(requestIdFromLine(line), wait) },
    ]);
    try {
      const client = await connectClient(daemon, undefined, {
        origin: { sessionId: "s-origin", daemonId: "daemon-origin" },
      });
      await client.sessionScreen({ session_id: "s-target" });
      await client.sessionInput({
        session_id: "s-target",
        text: "hello",
        wait: { until: ["idle"] },
      });
      await client.sessionOutput({
        session_id: "s-target",
        runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
        after_offset: "0",
        max_bytes: 128,
        wait_ms: 25,
      });
      await client.sessionWait({
        session_id: "s-target",
        after_updated_at: "2026-08-04T00:00:00Z",
        timeout_ms: 25,
      });

      for (const method of ["session.screen", "session.input", "session.output", "session.wait"]) {
        const sent = parseRequestLine(await daemon.nextRequest());
        expect(sent["method"]).toBe(method);
        expect(sent["origin_session_id"]).toBe("s-origin");
        expect(sent["origin_daemon_id"]).toBe("daemon-origin");
      }
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("input wait overall deadline uses a dedicated connection with headroom", async () => {
    const input = {
      accepted: true,
      activity: "idle",
      activity_source: "screen",
      runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
      activity_epoch: "d-epoch-1",
      activity_revision: "2",
    } as const;
    const daemon = await startUnixDaemon([
      {
        kind: "delay",
        ms: 60,
        line: (line) => okResponseLine(requestIdFromLine(line), input),
      },
      {
        kind: "reply",
        line: (line) => okResponseLine(requestIdFromLine(line), {
          status: "ok",
          daemon_version: "test",
          protocol_version: PROTOCOL_VERSION,
        }),
      },
    ]);
    try {
      const client = await connectClient(daemon, undefined, { requestTimeoutMs: 20 });

      expect(await client.sessionInput({
        session_id: "s-target",
        text: "hello",
        wait: { until: ["idle"] },
      })).toEqual(input);
      expect(await client.call("daemon.health", null)).toEqual({
        status: "ok",
        daemon_version: "test",
        protocol_version: PROTOCOL_VERSION,
      });

      const waiting = parseRequestLine(await daemon.nextRequest());
      expect(waiting["method"]).toBe("session.input");
      expect(waiting["params"]).toEqual({
        session_id: "s-target",
        text: "hello",
        wait: { until: ["idle"] },
      });
      const followUp = parseRequestLine(await daemon.nextRequest());
      expect(followUp["method"]).toBe("daemon.health");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("input wait rejects a legacy success without runtime evidence", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (line) => okResponseLine(requestIdFromLine(line), { accepted: true }),
      },
    ]);
    try {
      const client = await connectClient(daemon);
      const error = await expectClientError(client.sessionInput({
        session_id: "s-target",
        text: "hello",
        wait: { until: ["idle"] },
      }));

      expect(error.toProtocolError().code).toBe("session_input_wait_contract_mismatch");
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("input wait rejects malformed runtime-scoped evidence", async () => {
    const malformedResults = [
      {
        accepted: true,
        activity: "idle",
        activity_source: "report",
        runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
        activity_revision: "2",
      },
      {
        accepted: true,
        activity: "idle",
        activity_source: "unknown",
        runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
        activity_epoch: "d-epoch-1",
        activity_revision: "2",
      },
      {
        accepted: true,
        activity: "idle",
        activity_source: "report",
        runtime: { runtime_id: "runtime-target", runtime_generation: "1" },
        activity_epoch: "d-epoch-1",
        activity_revision: "18446744073709551616",
      },
    ];

    for (const result of malformedResults) {
      const daemon = await startUnixDaemon([
        {
          kind: "reply",
          line: (line) => okResponseLine(requestIdFromLine(line), result),
        },
      ]);
      try {
        const client = await connectClient(daemon);
        const error = await expectClientError(client.sessionInput({
          session_id: "s-target",
          text: "hello",
          wait: { until: ["idle"] },
        }));

        expect(error.toProtocolError().code).toBe("session_input_wait_contract_mismatch");
        await client.close();
      } finally {
        await daemon.close();
      }
    }
  });

  test("partial request origins fail before any wire write", async () => {
    const daemon = await startUnixDaemon([{ kind: "silent" }]);
    try {
      const client = await connectClient(daemon);
      const request: Request = {
        v: SUPPORTED_PROTOCOL_VERSIONS,
        id: "partial-origin",
        method: "daemon.health",
        params: null,
        origin_session_id: "s-origin",
      };

      const error = await expectClientError(client.request(request));
      expect(error.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("configured origin rejects a conflicting complete request origin before write", async () => {
    const daemon = await startUnixDaemon([{ kind: "silent" }]);
    try {
      const client = await connectClient(daemon, undefined, {
        origin: { sessionId: "s-origin", daemonId: "daemon-origin" },
      });
      const request: Request = {
        v: SUPPORTED_PROTOCOL_VERSIONS,
        id: "conflicting-origin",
        method: "daemon.health",
        params: null,
        origin_session_id: "s-other",
        origin_daemon_id: "daemon-other",
      };

      const error = await expectClientError(client.request(request));
      expect(error.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
      await client.close();
    } finally {
      await daemon.close();
    }
  });

  test("invalid configured origins fail closed", async () => {
    const daemon = await startUnixDaemon([{ kind: "silent" }]);
    try {
      const invalid = {
        origin: { sessionId: "s-origin" },
      } as unknown as ConnectOptions;
      try {
        await connectClient(daemon, undefined, invalid);
        throw new Error("expected invalid origin to reject");
      } catch (error: unknown) {
        expect(error).toBeInstanceOf(TypeError);
      }
      await daemon.expectNoRequest(50);
    } finally {
      await daemon.close();
    }
  });

  test("request timeout rejects and poisons the connection", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "delay",
        ms: 60,
        line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { status: "ok" }),
      },
    ]);
    try {
      const client = await connectClient(daemon, undefined, { requestTimeoutMs: 20 });

      const firstError = await expectClientError(client.call("daemon.health", null));
      expect(firstError.toProtocolError().class).toBe("transport");
      expect(firstError.toProtocolError().code).toBe("request_timeout");
      await daemon.nextRequest();

      const secondError = await expectClientError(client.call("daemon.health", null));
      expect(secondError.toProtocolError().class).toBe("transport");
      expect(secondError.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
    } finally {
      await daemon.close();
    }
  });

  test("connect options default to 5000ms", () => {
    expect(Client.defaultOptions()).toEqual({
      connectTimeoutMs: 5000,
      requestTimeoutMs: 5000,
    });
  });

  test("close shuts down the control channel and prevents later requests", async () => {
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) =>
          okResponseLine(requestIdFromLine(requestLine), {
            status: "ok",
            daemon_version: "0.0.0-test",
            protocol_version: PROTOCOL_VERSION,
          }),
      },
    ]);
    try {
      const client = await connectClient(daemon);

      await client.close();
      const error = await expectClientError(client.call("daemon.health", null));

      expect(error.toProtocolError().class).toBe("transport");
      expect(error.toProtocolError().code).toBe("framing");
      await daemon.expectNoRequest(50);
    } finally {
      await daemon.close();
    }
  });

  test("local daemon unreachable maps to daemon_unreachable with recovery hint", async () => {
    const clientPromise = connectLocal("/tmp/pohunek-sdk-missing-daemon.sock");

    const error = await expectClientError(clientPromise);

    const structured = error.toProtocolError();
    expect(structured.class).toBe("daemon");
    expect(structured.code).toBe("daemon_unreachable");
    expect(structured.recover).toContain("daemon start");
  });
});

async function connectClient(
  daemon: MockDaemon,
  host?: string,
  opts?: ConnectOptions,
): Promise<Client> {
  if (daemon.endpoint.kind === "unix") {
    return connectLocal(daemon.endpoint.socketPath, opts);
  }
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport, opts, host);
  }
  return connectTcp(host ?? "build-box", { host: daemon.endpoint.host, port: daemon.endpoint.port }, opts);
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
