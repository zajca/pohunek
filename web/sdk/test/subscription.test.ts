import { describe, expect, test } from "bun:test";
import { PROTOCOL_VERSION, type ProtocolEvent } from "@pohunek/protocol";
import { Client, ClientError, type CatchAllEvent, type Request } from "@pohunek/sdk";
import {
  errResponseLine,
  minimalSessionInfo,
  okResponseLine,
  requestIdFromLine,
  startUnixDaemon,
} from "./mock-daemon";

describe("Subscription", () => {
  test("nextEvent yields typed events, catch-all unknown events, and null on close", async () => {
    const request = subscribeRequest("subscribe-events");
    const events = [
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "session_created",
        session: minimalSessionInfo(),
      }),
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "agent_state",
        session_id: "s-test-1",
        activity: "blocked",
        source: "report",
      }),
      JSON.stringify({
        v: PROTOCOL_VERSION,
        event: "future_event",
        id: "req-future",
        payload: "still visible",
      }),
    ];
    const daemon = await startUnixDaemon([
      {
        kind: "subscription",
        ack: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { subscribed: true }),
        events,
      },
    ]);
    try {
      const client = await connectClient(daemon);
      const subscription = await client.subscribe(request);

      const first = await subscription.nextEvent();
      expect((first as ProtocolEvent | null)?.event).toBe("session_created");
      if (first?.event === "session_created") {
        const sessionEvent = first as Extract<ProtocolEvent, { event: "session_created" }>;
        expect(sessionEvent.session.id).toBe("s-test-1");
      }

      const second = await subscription.nextEvent();
      expect((second as ProtocolEvent | null)?.event).toBe("agent_state");
      if (second?.event === "agent_state") {
        expect(second.activity).toBe("blocked");
        expect(second.source).toBe("report");
      }

      const third = await subscription.nextEvent();
      if (!isCatchAllEvent(third)) {
        throw new Error("expected unknown event to decode as catch-all");
      }
      expect(third.event).toBe("future_event");
      expect(catchAllPayload(third)).toBe("still visible");

      expect(await subscription.nextEvent()).toBeNull();
    } finally {
      await daemon.close();
    }
  });

  test("nextLine returns raw event lines and null on close", async () => {
    const request = subscribeRequest("subscribe-lines");
    const eventLine = JSON.stringify({
      v: PROTOCOL_VERSION,
      event: "agent_state",
      session_id: "s-test-1",
      activity: "working",
      source: "report",
    });
    const daemon = await startUnixDaemon([
      {
        kind: "subscription",
        ack: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { subscribed: true }),
        events: [eventLine],
      },
    ]);
    try {
      const client = await connectClient(daemon);
      const subscription = await client.subscribe(request);

      expect(await subscription.nextLine()).toBe(eventLine);
      expect(await subscription.nextLine()).toBeNull();
    } finally {
      await daemon.close();
    }
  });

  test("subscription ack errors preserve protocol class and code", async () => {
    const request = subscribeRequest("subscribe-error");
    const daemonError = {
      class: "runtime",
      code: "subscription_denied",
      msg: "subscription denied during test",
      recover: "retry later",
    } as const;
    const daemon = await startUnixDaemon([
      {
        kind: "reply",
        line: (requestLine) => errResponseLine(requestIdFromLine(requestLine), daemonError),
      },
    ]);
    try {
      const client = await connectClient(daemon);

      const error = await expectClientError(client.subscribe(request));

      expect(error.toProtocolError().class).toBe(daemonError.class);
      expect(error.toProtocolError().code).toBe(daemonError.code);
      expect(error.toProtocolError().recover).toBe(daemonError.recover);
    } finally {
      await daemon.close();
    }
  });

  test("malformed event JSON maps to json_error", async () => {
    const request = subscribeRequest("subscribe-malformed");
    const daemon = await startUnixDaemon([
      {
        kind: "subscription",
        ack: (requestLine) => okResponseLine(requestIdFromLine(requestLine), { subscribed: true }),
        events: ["definitely not json"],
      },
    ]);
    try {
      const client = await connectClient(daemon);
      const subscription = await client.subscribe(request);

      const error = await expectClientError(subscription.nextEvent());

      expect(error.toProtocolError().class).toBe("daemon");
      expect(error.toProtocolError().code).toBe("json_error");
    } finally {
      await daemon.close();
    }
  });
});

function subscribeRequest(id: string): Request {
  return {
    v: PROTOCOL_VERSION,
    id,
    method: "subscribe",
    params: null,
  };
}

function isCatchAllEvent(event: ProtocolEvent | CatchAllEvent | null): event is CatchAllEvent {
  return event !== null && event.event === "future_event";
}

function catchAllPayload(event: CatchAllEvent): unknown {
  const payload = event["payload"];
  return payload;
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

async function connectClient(daemon: Awaited<ReturnType<typeof startUnixDaemon>>): Promise<Client> {
  if (daemon.endpoint.kind === "unix") {
    return Client.connectLocal(daemon.endpoint.socketPath);
  }
  if (daemon.endpoint.kind === "memory") {
    return Client.connectTransport(daemon.endpoint.transport);
  }
  return Client.connectTcp("build-box", { host: daemon.endpoint.host, port: daemon.endpoint.port });
}
