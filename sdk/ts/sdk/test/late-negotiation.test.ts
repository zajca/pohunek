import { describe, expect, test } from "bun:test";
import { CLIENT_PROTOCOL_VERSIONS, Client, ClientError, PROTOCOL_VERSION, nextRequestId } from "@pohunek/sdk";
import type { ControlChannel, Transport } from "@pohunek/sdk";

/**
 * A request that waits for the version probe must not be written after its
 * caller gave up. The channel is driven by the test, so the late probe reply is
 * delivered at a point the test chooses.
 */
class ScriptedChannel implements ControlChannel {
  public readonly sent: string[] = [];
  private readonly queue: string[] = [];
  private waiting: ((value: IteratorResult<string>) => void) | undefined;
  private done = false;

  public readonly lines: AsyncIterable<string> = {
    [Symbol.asyncIterator]: (): AsyncIterator<string> => ({
      next: (): Promise<IteratorResult<string>> => {
        const line = this.queue.shift();
        if (line !== undefined) {
          return Promise.resolve({ value: line, done: false });
        }
        if (this.done) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise((resolve) => {
          this.waiting = resolve;
        });
      },
    }),
  };

  public send(line: string): Promise<void> {
    this.sent.push(line);
    return Promise.resolve();
  }

  public deliver(line: string): void {
    const waiting = this.waiting;
    if (waiting === undefined) {
      this.queue.push(line);
      return;
    }
    this.waiting = undefined;
    waiting({ value: line, done: false });
  }

  public close(): Promise<void> {
    this.done = true;
    this.waiting?.({ value: undefined, done: true });
    return Promise.resolve();
  }
}

function transportOver(channel: ControlChannel): Transport {
  return {
    control: () => Promise.resolve(channel),
    raw: () => Promise.reject(new Error("no raw stream")),
  };
}

function methodsSent(channel: ScriptedChannel): string[] {
  return channel.sent.map((line) => (JSON.parse(line) as { method: string }).method);
}

/** Lets every already-queued continuation run; it waits on no clock. */
function flush(): Promise<void> {
  return new Promise((resolve) => {
    setImmediate(resolve);
  });
}

const UNINSTALL = { digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000" };

describe("a request that waits for the version probe", () => {
  test("is not written when the probe answers after the caller timed out", async () => {
    const channel = new ScriptedChannel();
    const client = await Client.connectTransport(transportOver(channel), { requestTimeoutMs: 1 });
    const outcome = client
      .request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("package.uninstall"), method: "package.uninstall", params: UNINSTALL })
      .then(() => undefined, (error: unknown) => error);

    // package.uninstall needs the version, so only the probe is written.
    expect(methodsSent(channel)).toEqual(["daemon.health"]);
    const error = await outcome;
    expect(error).toBeInstanceOf(ClientError);
    expect((error as ClientError).kind).toBe("requestTimeout");

    // The probe answers late, with a version that would let the request through.
    const probe = JSON.parse(channel.sent[0] ?? "{}") as { id: string };
    channel.deliver(JSON.stringify({
      v: PROTOCOL_VERSION,
      id: probe.id,
      ok: { status: "ok", daemon_version: "0.0.0", protocol_version: PROTOCOL_VERSION },
    }));
    await flush();

    expect(methodsSent(channel)).toEqual(["daemon.health"]);
    const retry = await client.request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("daemon.health"), method: "daemon.health", params: null })
      .then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(retry?.kind).toBe("framing");
    expect(methodsSent(channel)).toEqual(["daemon.health"]);
  });

  test("is not written when the client was closed while the probe was in flight", async () => {
    const channel = new ScriptedChannel();
    const client = await Client.connectTransport(transportOver(channel), { requestTimeoutMs: 60_000 });
    const outcome = client
      .request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("package.uninstall"), method: "package.uninstall", params: UNINSTALL })
      .then(() => undefined, (error: unknown) => error);
    const probe = JSON.parse(channel.sent[0] ?? "{}") as { id: string };
    await client.close();
    channel.deliver(JSON.stringify({
      v: PROTOCOL_VERSION,
      id: probe.id,
      ok: { status: "ok", daemon_version: "0.0.0", protocol_version: PROTOCOL_VERSION },
    }));
    await outcome;
    await flush();
    expect(methodsSent(channel)).toEqual(["daemon.health"]);
  });
});

describe("caller-owned parameters", () => {
  test("a request keeps the parameters it was invoked with", async () => {
    const channel = new ScriptedChannel();
    const client = await Client.connectTransport(transportOver(channel), { requestTimeoutMs: 60_000 });
    const params = { filters: [] as unknown[], note: "original" };
    const outcome = client
      .request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("session.list"), method: "session.list", params })
      .then(() => undefined, (error: unknown) => error);
    params.note = "changed";
    params.filters.push("changed");
    await flush();
    const sent = JSON.parse(channel.sent[0] ?? "{}") as { params: unknown };
    expect(sent.params).toEqual({ filters: [], note: "original" });
    await client.close();
    await outcome;
  });

  test("a request keeps its parameters while the version probe is pending", async () => {
    const channel = new ScriptedChannel();
    const client = await Client.connectTransport(transportOver(channel), { requestTimeoutMs: 60_000 });
    const params = { digest: UNINSTALL.digest };
    const outcome = client
      .request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("package.uninstall"), method: "package.uninstall", params })
      .then(() => undefined, (error: unknown) => error);
    expect(methodsSent(channel)).toEqual(["daemon.health"]);
    params.digest = "sha256:changed";
    const probe = JSON.parse(channel.sent[0] ?? "{}") as { id: string };
    channel.deliver(JSON.stringify({
      v: PROTOCOL_VERSION,
      id: probe.id,
      ok: { status: "ok", daemon_version: "0.0.0", protocol_version: PROTOCOL_VERSION },
    }));
    await flush();
    const sent = JSON.parse(channel.sent[1] ?? "{}") as { method: string; params: { digest: string } };
    expect(sent.method).toBe("package.uninstall");
    expect(sent.params.digest).toBe(UNINSTALL.digest);
    // A request the connection has already selected a version for is pinned to it.
    expect((JSON.parse(channel.sent[1] ?? "{}") as { v: unknown }).v).toEqual({
      minimum: PROTOCOL_VERSION,
      maximum: PROTOCOL_VERSION,
    });
    await client.close();
    await outcome;
  });

  test("a dedicated call keeps the parameters it was invoked with", async () => {
    const parent = new ScriptedChannel();
    const dedicated = new ScriptedChannel();
    let release: (() => void) | undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let connections = 0;
    const transport: Transport = {
      control: async () => {
        connections += 1;
        if (connections === 1) {
          return parent;
        }
        await gate;
        return dedicated;
      },
      raw: () => Promise.reject(new Error("no raw stream")),
    };
    const client = await Client.connectTransport(transport, { requestTimeoutMs: 60_000 });
    const params = { session_id: "s-1", timeout_ms: 100 } as unknown as Parameters<Client["sessionWait"]>[0];
    const outcome = client.sessionWait(params).then(() => undefined, (error: unknown) => error);
    (params as unknown as { session_id: string }).session_id = "s-changed";
    release?.();
    await flush();
    const sent = JSON.parse(dedicated.sent[0] ?? "{}") as { params: { session_id: string } };
    expect(sent.params.session_id).toBe("s-1");
    await dedicated.close();
    await outcome;
  });
});
