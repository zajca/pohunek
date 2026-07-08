import { describe, expect, test } from "bun:test";
import { MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION, type ProtocolError, type SessionInfo } from "@pohunek/protocol";
import { Client, ClientError, type Request } from "@pohunek/sdk";
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
      expect(sent["v"]).toBe(PROTOCOL_VERSION);
      expect(sent["method"]).toBe("session.list");
      expect(sent["params"]).toEqual({ filters: [{ key: "state", value: "running" }] });
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
        v: PROTOCOL_VERSION,
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
      expect(firstError.toProtocolError().code).toBe("framing");
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
    const clientPromise = Client.connectLocal("/tmp/pohunek-sdk-missing-daemon.sock");

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
  opts?: { connectTimeoutMs?: number; requestTimeoutMs?: number },
): Promise<Client> {
  if (daemon.endpoint.kind === "unix") {
    return Client.connectLocal(daemon.endpoint.socketPath, opts);
  }
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport, opts, host);
  }
  return Client.connectTcp(host ?? "build-box", { host: daemon.endpoint.host, port: daemon.endpoint.port }, opts);
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
