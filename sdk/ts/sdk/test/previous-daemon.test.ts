import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import {
  CLIENT_PROTOCOL_VERSIONS,
  ClientError,
  METHOD_NAMES,
  MIN_PROTOCOL_VERSION,
  PREVIOUS_VERSION_INTRODUCED_METHODS,
  PROTOCOL_VERSION,
  connectTcp,
  nextRequestId,
  type Client,
  type ProtocolEvent,
  type SessionInputParams,
  type SessionOutputParams,
  type SessionScreenParams,
  type SessionWaitParams,
} from "@pohunek/sdk";
import {
  containsKey,
  previousKeys,
  recordedEvents,
  recordedRequests,
  recordedResults,
  renameKeysEverywhere,
} from "./previous-release-fixtures";

/**
 * A client of this build against a daemon of the previous release.
 *
 * The stub negotiates protocol 3 only, accepts a request only when its
 * parameters equal one recorded from that release and answers with the recorded
 * result of the method. The expected current shape is an independent blind key
 * rename of the recorded payload.
 */

const PREVIOUS = MIN_PROTOCOL_VERSION;
const HOST = "netbird:previous-release";

interface Seen {
  exchanges: Array<{ connection: number; method: string; params: unknown }>;
  violations: string[];
  connections: number;
  /** Version range each accepted request advertised. */
  ranges: Array<{ method: string; range: { minimum: number; maximum: number } }>;
}

interface PreviousDaemon {
  port: number;
  seen: Seen;
  methods(): string[];
  /** Makes the daemon serve the current version too, or only the previous one. */
  serveCurrent(enabled: boolean): void;
  close(): Promise<void>;
}

const open: PreviousDaemon[] = [];

afterEach(async () => {
  while (open.length > 0) {
    await open.pop()?.close();
  }
});

function startPreviousDaemon(
  resultOverrides: Record<string, unknown> = {},
  /** Also negotiates the current version, like a daemon of the current release. */
  servesCurrent = false,
  extraRequests: Array<{ method: string; params: unknown }> = [],
): Promise<PreviousDaemon> {
  let ownMaximum = servesCurrent ? PROTOCOL_VERSION : PREVIOUS;
  const requests = recordedRequests();
  const results = recordedResults();
  const events = recordedEvents();
  const seen: Seen = { exchanges: [], violations: [], connections: 0, ranges: [] };
  const sockets = new Set<Socket>();

  const reply = (socket: Socket, value: unknown): void => {
    socket.write(`${JSON.stringify(value)}\n`);
  };

  const server: Server = createServer({ allowHalfOpen: true }, (socket) => {
    sockets.add(socket);
    seen.connections += 1;
    const connection = seen.connections;
    let buffer = "";
    socket.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      for (;;) {
        const end = buffer.indexOf("\n");
        if (end < 0) {
          return;
        }
        const line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        handle(socket, connection, JSON.parse(line) as Record<string, unknown>);
      }
    });
    socket.on("error", () => undefined);
    socket.on("close", () => sockets.delete(socket));
  });

  const handle = (socket: Socket, connection: number, request: Record<string, unknown>): void => {
    const range = request["v"] as { minimum: number; maximum: number };
    const id = request["id"] as string;
    const method = request["method"] as string;
    if (Math.max(range.minimum, PREVIOUS) > Math.min(range.maximum, ownMaximum)) {
      reply(socket, { v: ownMaximum, id, err: { class: "daemon", code: "version_mismatch", msg: "no overlap" } });
      return;
    }
    const version = Math.min(range.maximum, ownMaximum);
    seen.exchanges.push({ connection, method, params: request["params"] ?? null });
    seen.ranges.push({ method, range });
    const recorded = requests.filter((entry) => entry.method === method);
    if (recorded.length === 0) {
      seen.violations.push(`${method} is not defined in protocol ${PREVIOUS}`);
      reply(socket, { v: version, id, err: { class: "daemon", code: "method_not_found", msg: method } });
      return;
    }
    const accepted = [...recorded, ...extraRequests.filter((entry) => entry.method === method)];
    if (!accepted.some((entry) => canonical(entry.params) === canonical(request["params"] ?? null))) {
      seen.violations.push(`${method} carried parameters protocol ${PREVIOUS} never recorded: ${JSON.stringify(request["params"])}`);
      reply(socket, { v: version, id, err: { class: "daemon", code: "bad_request", msg: "parameters are not valid for this protocol" } });
      return;
    }
    const result = method in resultOverrides
      ? resultOverrides[method]
      : results.find((entry) => entry.method === method)?.result;
    reply(socket, { v: version, id, ok: result });
    if (method === "subscribe") {
      for (const event of events) {
        reply(socket, { v: version, event: event.event, ...event.payload });
      }
      socket.end();
    }
  };

  return new Promise((resolve) => {
    server.listen({ host: "127.0.0.1", port: 0 }, () => {
      const daemon: PreviousDaemon = {
        port: (server.address() as AddressInfo).port,
        seen,
        methods: () => seen.exchanges.map((exchange) => exchange.method),
        serveCurrent: (enabled: boolean): void => {
          ownMaximum = enabled ? PROTOCOL_VERSION : PREVIOUS;
        },
        close: () => new Promise<void>((done) => {
          for (const socket of sockets) {
            socket.destroy();
          }
          server.close(() => done());
        }),
      };
      open.push(daemon);
      resolve(daemon);
    });
  });
}

/** Key-order independent serialization, so only the content is compared. */
function canonical(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(canonical).join(",")}]`;
  }
  if (typeof value === "object" && value !== null) {
    const body = Object.entries(value)
      .sort(([left], [right]) => (left < right ? -1 : 1))
      .map(([key, inner]) => `${JSON.stringify(key)}:${canonical(inner)}`);
    return `{${body.join(",")}}`;
  }
  return JSON.stringify(value);
}

function connect(daemon: PreviousDaemon): Promise<Client> {
  return connectTcp(HOST, { host: "127.0.0.1", port: daemon.port });
}

function rawRequest(client: Client, method: string, params: unknown): Promise<unknown> {
  return client.request({ v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId(method), method, params });
}

function recordedExchanges(): Array<{ method: string; params: unknown; expected: unknown }> {
  const results = recordedResults();
  return recordedRequests().map(({ method, params }) => {
    const result = results.find((entry) => entry.method === method);
    if (result === undefined) {
      throw new Error(`no recorded result for ${method}`);
    }
    return { method, params: renameKeysEverywhere(params), expected: renameKeysEverywhere(result.result) };
  });
}

async function everyRecordedMethod(fresh: boolean): Promise<void> {
  const daemon = await startPreviousDaemon();
  const shared = await connect(daemon);
  const exercised = new Set<string>();
  for (const { method, params, expected } of recordedExchanges()) {
    exercised.add(method);
    if (method === "subscribe") {
      continue;
    }
    const client = fresh ? await connect(daemon) : shared;
    const originalParams = JSON.stringify(params);
    const value = await rawRequest(client, method, params);
    expect(value).toEqual(expected);
    expect(JSON.stringify(params)).toBe(originalParams);
    for (const key of previousKeys()) {
      expect(containsKey(value, key)).toBe(false);
    }
    if (fresh) {
      await client.close();
    }
  }
  await shared.close();
  expect(daemon.seen.violations).toEqual([]);

  const introduced = new Set<string>(PREVIOUS_VERSION_INTRODUCED_METHODS);
  const previousMethods = new Set(METHOD_NAMES.filter((name) => !introduced.has(name)));
  expect(exercised).toEqual(previousMethods);
}

describe("client against a daemon of the previous release", () => {
  test("a previous daemon leaves native transcript activity unknown", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const recorded = recordedExchanges().find((entry) => entry.method === "session.inspect");
    if (recorded === undefined) {
      throw new Error("no session.inspect recording");
    }
    const response = await rawRequest(client, "session.inspect", recorded.params) as Record<string, unknown>;
    expect(response["native_last_activity_at"]).toBeUndefined();
    expect(response).toEqual(recorded.expected);
  });

  test("every previous-release method works on one connection", async () => {
    await everyRecordedMethod(false);
  });

  test("every previous-release method works on a cold connection", async () => {
    await everyRecordedMethod(true);
  });

  test("a request that is the same in both versions is sent without a probe", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const list = recordedExchanges().find((entry) => entry.method === "session.list");
    await rawRequest(client, "session.list", list?.params);
    expect(daemon.methods()).toEqual(["session.list"]);
    expect(daemon.seen.violations).toEqual([]);
  });

  test("a version-dependent request learns the version before it is sent", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const output = recordedExchanges().find(
      (entry) => entry.method === "session.output" && containsKey(entry.params, "worker_instance_id"),
    );
    expect(await rawRequest(client, "session.output", output?.params)).toEqual(output?.expected);
    expect(daemon.methods()).toEqual(["daemon.health", "session.output"]);
    expect(daemon.seen.violations).toEqual([]);
  });

  test("a previous-only request settles the connection on the previous version", async () => {
    // A daemon that also serves the current version would select it for a probe
    // that offered the whole window.
    const daemon = await startPreviousDaemon({}, true);
    const client = await connect(daemon);
    const output = recordedExchanges().find(
      (entry) => entry.method === "session.output" && containsKey(entry.params, "worker_instance_id"),
    );
    const request = {
      v: { minimum: PREVIOUS, maximum: PREVIOUS },
      id: nextRequestId("session.output"),
      method: "session.output",
      params: output?.params,
    };
    expect(await client.request(request)).toEqual(output?.expected);
    expect(daemon.seen.violations).toEqual([]);
    // The connection settled on the previous version: a current-only request is
    // refused client-side and nothing more is sent.
    const sentBefore = daemon.methods().length;
    const error = await client.request({ ...request, id: nextRequestId("session.output"), v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION } })
      .then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(error?.kind).toBe("daemonProtocolTooOld");
    expect(error?.tooOld?.requiredVersion).toBe(PROTOCOL_VERSION);
    expect(daemon.methods().length).toBe(sentBefore);
  });

  test("a current-only request against a previous daemon is refused before it is sent", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const output = recordedExchanges().find(
      (entry) => entry.method === "session.output" && containsKey(entry.params, "worker_instance_id"),
    );
    const error = await client.request({
      v: { minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION },
      id: nextRequestId("session.output"),
      method: "session.output",
      params: output?.params,
    }).then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(error?.toProtocolError().code).toBe("version_mismatch");
    expect(daemon.methods()).toEqual([]);
    expect(daemon.seen.violations).toEqual([]);
  });

  test("a request whose range misses the window is refused without a probe", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const error = await client.request({
      v: { minimum: PROTOCOL_VERSION + 1, maximum: PROTOCOL_VERSION + 1 },
      id: nextRequestId("package.list"),
      method: "package.list",
      params: null,
    }).then(() => undefined, (caught: unknown) => caught as ClientError);
    expect(error?.toProtocolError().code).toBe("version_mismatch");
    expect(daemon.methods()).toEqual([]);
  });

  test("a dedicated mutation is refused by a daemon rolled back after the parent selected", async () => {
    const daemon = await startPreviousDaemon({}, true);
    const client = await connect(daemon);
    const list = recordedExchanges().find((entry) => entry.method === "session.list");
    await rawRequest(client, "session.list", list?.params);

    // The daemon restarts as the previous release; the waiting input runs on a
    // dedicated connection that inherits the parent's current version.
    daemon.serveCurrent(false);
    const input = recordedExchanges().find((entry) => entry.method === "session.input");
    const error = await client.sessionInput(input?.params as SessionInputParams).then(
      () => undefined,
      (caught: unknown) => caught as ClientError,
    );
    expect(error?.toProtocolError().code).toBe("version_mismatch");
    expect(daemon.methods()).toEqual(["session.list"]);
  });

  test("host discovery keeps the whole window after the connection selected a version", async () => {
    const daemon = await startPreviousDaemon({}, true);
    const client = await connect(daemon);
    // The first reply selects the current version on this connection.
    await rawRequest(client, "daemon.health", null);
    const discover = recordedExchanges().find((entry) => entry.method === "host.discover");
    await rawRequest(client, "host.discover", discover?.params);
    const list = recordedExchanges().find((entry) => entry.method === "session.list");
    await rawRequest(client, "session.list", list?.params);

    const rangeOf = (method: string): unknown => daemon.seen.ranges.find((entry) => entry.method === method)?.range;
    // The daemon probes peers for the range the request advertised.
    expect(rangeOf("host.discover")).toEqual(CLIENT_PROTOCOL_VERSIONS);
    expect(rangeOf("session.list")).toEqual({ minimum: PROTOCOL_VERSION, maximum: PROTOCOL_VERSION });
    expect(daemon.seen.violations).toEqual([]);
  });

  test("the handshake reports the previous version", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    expect(await client.handshake()).toBe(PREVIOUS);
  });

  test("metadata keys remain caller data across a previous-version exchange", async () => {
    const method = "session.set_metadata";
    const params = {
      session_id: "s-42",
      metadata: { worker_instance_id: "user-value" },
    };
    const recorded = recordedResults().find((entry) => entry.method === method);
    if (recorded === undefined) {
      throw new Error(`no ${method} result recording`);
    }
    const session = (recorded.result as { session: Record<string, unknown> }).session;
    const result = {
      session: {
        ...session,
        metadata: { ...(session["metadata"] as Record<string, unknown>), runtime_id: "user-value" },
      },
    };
    const daemon = await startPreviousDaemon({ [method]: result }, false, [{ method, params }]);
    const client = await connect(daemon);

    const response = await rawRequest(client, method, params) as { session: { metadata: Record<string, unknown> } };
    expect(daemon.seen.exchanges.find((entry) => entry.method === method)?.params).toEqual(params);
    expect(response.session.metadata["runtime_id"]).toBe("user-value");
    expect(response.session.metadata["worker_instance_id"]).toBeUndefined();
    expect(daemon.seen.violations).toEqual([]);
  });

  test("typed calls, including dedicated connections, decode the translated results", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const exchanges = recordedExchanges();
    const find = (method: string, withRuntime?: boolean): { method: string; params: unknown; expected: unknown } => {
      const found = exchanges.find((entry) =>
        entry.method === method
        && (withRuntime === undefined || containsKey(entry.params, "worker_instance_id") === withRuntime));
      if (found === undefined) {
        throw new Error(`no ${method} recording`);
      }
      return found;
    };

    const screen = find("session.screen");
    expect(await client.sessionScreen(screen.params as SessionScreenParams)).toEqual(screen.expected);
    const plain = find("session.output", false);
    expect(await client.sessionOutput(plain.params as SessionOutputParams)).toEqual(plain.expected);
    // Waiting reads use a dedicated connection that inherits the selected version.
    const waiting = find("session.output", true);
    expect(await client.sessionOutput(waiting.params as SessionOutputParams)).toEqual(waiting.expected);
    const wait = find("session.wait");
    expect(await client.sessionWait(wait.params as SessionWaitParams)).toEqual(wait.expected);

    expect(daemon.seen.violations).toEqual([]);
    expect(daemon.methods().filter((method) => method === "daemon.health")).toEqual([]);
  });

  test("a dedicated first call learns the version on its own connection", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const wait = recordedExchanges().find((entry) => entry.method === "session.wait");
    expect(await client.sessionWait(wait?.params as SessionWaitParams)).toEqual(wait?.expected);
    expect(daemon.methods()).toEqual(["daemon.health", "session.wait"]);
    expect(daemon.seen.violations).toEqual([]);
  });

  test("subscription events arrive in the current shape through nextEvent and nextLine", async () => {
    const daemon = await startPreviousDaemon();
    const recorded = recordedEvents();
    const request = { v: CLIENT_PROTOCOL_VERSIONS, id: nextRequestId("subscribe"), method: "subscribe", params: null };

    const typed = await (await connect(daemon)).subscribe(request);
    for (const entry of recorded) {
      const event = (await typed.nextEvent()) as ProtocolEvent | null;
      expect(event).toEqual({ v: PROTOCOL_VERSION, event: entry.event, ...(renameKeysEverywhere(entry.payload) as object) });
    }
    expect(await typed.nextEvent()).toBeNull();

    const lines = await (await connect(daemon)).subscribe({ ...request, id: nextRequestId("subscribe") });
    for (const entry of recorded) {
      const line = await lines.nextLine();
      expect(JSON.parse(line ?? "null")).toEqual({
        v: PROTOCOL_VERSION,
        event: entry.event,
        ...(renameKeysEverywhere(entry.payload) as object),
      });
    }
    expect(daemon.seen.violations).toEqual([]);
  });

  test("a method the previous release never defined fails before it is sent", async () => {
    expect(PREVIOUS_VERSION_INTRODUCED_METHODS.length).toBeGreaterThan(0);
    for (const method of PREVIOUS_VERSION_INTRODUCED_METHODS) {
      const daemon = await startPreviousDaemon();
      const client = await connect(daemon);
      const error = await rawRequest(client, method, null).then(
        () => undefined,
        (caught: unknown) => caught,
      );
      expect(error).toBeInstanceOf(ClientError);
      const failure = error as ClientError;
      expect(failure.kind).toBe("daemonProtocolTooOld");
      expect(failure.tooOld).toEqual({
        host: HOST,
        method,
        daemonVersion: PREVIOUS,
        requiredVersion: PROTOCOL_VERSION,
      });
      expect(daemon.methods()).toEqual(["daemon.health"]);
      expect(daemon.seen.violations).toEqual([]);
    }
  });

  test("the typed error names the host, the versions and the upgrade", async () => {
    const daemon = await startPreviousDaemon();
    const client = await connect(daemon);
    const error = await client.call("package.list", null).then(
      () => undefined,
      (caught: unknown) => caught as ClientError,
    );
    const structured = error?.toProtocolError();
    expect(structured?.class).toBe("daemon");
    expect(structured?.code).toBe("daemon_protocol_too_old");
    expect(structured?.msg).toBe(
      `host '${HOST}' runs protocol ${PREVIOUS}, but \`package.list\` needs protocol ${PROTOCOL_VERSION}`,
    );
    expect(structured?.recover).toContain(HOST);
    expect(structured?.recover).toContain(String(PROTOCOL_VERSION));
    expect(error?.message).toBe(structured?.msg);
    // The connection stays usable for methods both versions define.
    const list = recordedExchanges().find((entry) => entry.method === "session.list");
    expect(await rawRequest(client, "session.list", list?.params)).toEqual(list?.expected);
  });

  test("a result the previous release could not have sent is refused", async () => {
    const daemon = await startPreviousDaemon({
      "session.read": { runtime_id: "w-1", worker_instance_id: "w-2" },
    });
    const client = await connect(daemon);
    const read = recordedExchanges().find((entry) => entry.method === "session.read");
    const error = await rawRequest(client, "session.read", read?.params).then(
      () => undefined,
      (caught: unknown) => caught as ClientError,
    );
    expect(error?.kind).toBe("versionTranslation");
    expect(error?.toProtocolError().code).toBe("version_translation_failed");
  });
});
