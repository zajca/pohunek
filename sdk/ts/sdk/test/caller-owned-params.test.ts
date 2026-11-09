import { describe, expect, test } from "bun:test";
import {
  connectTcp,
  type SessionListFilter,
} from "@pohunek/sdk";
import {
  okResponseLine,
  parseRequestLine,
  requestIdFromLine,
  startTcpDaemon,
  type MockDaemon,
} from "./mock-daemon";

/**
 * A caller always keeps the parameters it invoked with. The scenario drives
 * the production client against a real TCP daemon and records what was written.
 */

const HOST = "netbird:caller-owned-params";

function address(daemon: MockDaemon): { host: string; port: number } {
  if (daemon.endpoint.kind !== "tcp") {
    throw new Error("caller-owned parameter tests require a real TCP endpoint");
  }
  return { host: daemon.endpoint.host, port: daemon.endpoint.port };
}

describe("caller-owned parameters over a real TCP daemon", () => {
  test("a request keeps the parameters it was invoked with", async () => {
    const daemon = await startTcpDaemon([
      { kind: "reply", line: (requestLine) => okResponseLine(requestIdFromLine(requestLine), []) },
    ]);
    try {
      const client = await connectTcp(HOST, address(daemon));
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
});
