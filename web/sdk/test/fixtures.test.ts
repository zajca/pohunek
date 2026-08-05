import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  decodeResponse,
  isErrResponse,
  isEvent,
  isOkResponse,
  isRequest,
  type Event,
  type Response,
} from "@pohunek/sdk";
import type {
  AgentStateEvent,
  AttachEvent,
  NotificationRecord,
  NotificationPolicy,
  ProtocolError,
  SessionEvent,
  SessionInfo,
  SessionListFilter,
  SessionOutputResult,
  SessionScreenResult,
  SessionWaitResult,
} from "@pohunek/protocol";

const fixturesDir = join(dirname(fileURLToPath(import.meta.url)), "../../shared/fixtures");

describe("golden fixtures", () => {
  test("every fixture is representable by the TS envelope and generated type layer", () => {
    const files = readdirSync(fixturesDir)
      .filter((file) => file.endsWith(".json"))
      .sort();

    expect(files.length).toBeGreaterThan(0);
    for (const file of files) {
      const value = readFixture(file);
      assertFixtureRepresentable(file, value);
    }
  });

  test("optional field fixtures preserve omission rather than null", () => {
    const minimal = readFixture("session-info-minimal.json") as SessionInfo;
    expect("external" in asRecord(minimal)).toBe(false);
    expect(minimal.external).toBeUndefined();
    expect("name" in asRecord(minimal)).toBe(false);
    expect(minimal.name).toBeUndefined();

    const full = readFixture("session-info-full.json") as SessionInfo;
    expect(full.external).toBe(false);
    expect(full.name).toBe("TypeScript SDK phase 1");
    expect(full.metadata).toEqual({ ticket: "POH-12", track: "S.3" });

    const withRecover = readFixture("protocol-error-with-recover.json") as ProtocolError;
    expect(withRecover.recover).toContain("pohunek doctor");

    const withoutRecover = readFixture("protocol-error-without-recover.json") as ProtocolError;
    expect("recover" in asRecord(withoutRecover)).toBe(false);
    expect(withoutRecover.recover).toBeUndefined();
  });
});

function readFixture(file: string): unknown {
  const text = readFileSync(join(fixturesDir, file), "utf8");
  return JSON.parse(text) as unknown;
}

function assertFixtureRepresentable(file: string, value: unknown): void {
  switch (file) {
    case "request-session-list.json":
    case "request-session-output.json":
    case "request-session-screen.json":
    case "request-session-wait.json":
      expect(isRequest(value)).toBe(true);
      return;
    case "response-ok-session-list.json": {
      const response: Response = decodeResponse(value);
      if (!isOkResponse(response)) {
        throw new Error("fixture did not decode to an ok response");
      }
      const sessions = response.ok as SessionInfo[];
      expect(sessions[0]?.id).toBe("s-fixture-1");
      return;
    }
    case "response-err-with-recover.json": {
      const response: Response = decodeResponse(value);
      if (!isErrResponse(response)) {
        throw new Error("fixture did not decode to an err response");
      }
      expect(response.err.recover).toContain("pohunek doctor");
      return;
    }
    case "event-agent-state.json":
      expect(isEvent(value)).toBe(true);
      expect((value as Event & AgentStateEvent).event).toBe("agent_state");
      expect((value as Event & AgentStateEvent).source).toBe("report");
      return;
    case "event-attach-opened-payload.json":
      expect((value as AttachEvent).stream_id).toBe("stream-fixture-1");
      return;
    case "event-session-created-payload.json":
      expect((value as SessionEvent).session.id).toBe("s-fixture-1");
      return;
    case "notification-record.json":
      expect((value as NotificationRecord).id).toBe("n-fixture-1");
      return;
    case "notification-policy-provider-keyed.json": {
      const policy = value as NotificationPolicy;
      expect(policy.providers?.["future-agent"]?.system).toBe(false);
      expect(policy.enabled.system).toBe(true);
      return;
    }
    case "protocol-error-with-recover.json":
      expect((value as ProtocolError).recover).toContain("pohunek doctor");
      return;
    case "protocol-error-without-recover.json":
      expect((value as ProtocolError).recover).toBeUndefined();
      return;
    case "session-info-full.json":
      expect((value as SessionInfo).warnings?.[0]?.kind).toBe("fetch");
      return;
    case "session-info-minimal.json":
      expect((value as SessionInfo).state).toBe("running");
      return;
    case "session-list-filter-state.json":
      expect((value as SessionListFilter).key).toBe("state");
      return;
    case "response-ok-session-screen.json": {
      const response = decodeResponse(value);
      if (!isOkResponse(response)) throw new Error("screen fixture was not successful");
      expect((response.ok as SessionScreenResult).runtime_generation).toBe("3");
      return;
    }
    case "response-ok-session-output-gap.json": {
      const response = decodeResponse(value);
      if (!isOkResponse(response)) throw new Error("output fixture was not successful");
      expect((response.ok as SessionOutputResult).gap?.end_offset).toBe("4");
      return;
    }
    case "response-ok-session-wait.json": {
      const response = decodeResponse(value);
      if (!isOkResponse(response)) throw new Error("wait fixture was not successful");
      expect((response.ok as SessionWaitResult).reason).toBe("runtime_changed");
      return;
    }
    default:
      throw new Error(`unhandled fixture: ${file}`);
  }
}

function asRecord(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("fixture value is not an object");
  }
  return value as Record<string, unknown>;
}
