import { describe, expect, test } from "bun:test";
import {
  agentKindLabel,
  agentRuntimeLabel,
  agentRuntimeStatus,
  hasAttachableSessionAgentBases,
  hasKnownSessionAgentBases,
  isLaunchableRuntime,
  sessionAgentLabel,
} from "../src/lib/agent-presentation";

describe("agent presentation", () => {
  test("labels Hermes and preserves an unknown future agent neutrally", () => {
    expect(agentKindLabel("hermes")).toBe("Hermes");
    expect(agentKindLabel("future-agent")).toBe("Unknown agent (future-agent)");
  });

  test("renders runtime inventory states without probe output", () => {
    expect(agentRuntimeStatus(undefined)).toBe("missing");
    expect(agentRuntimeStatus({ agent: "hermes", available: false })).toBe("missing");
    expect(agentRuntimeStatus({ agent: "hermes", available: true, supported: true })).toBe("installed (supported)");
    expect(agentRuntimeStatus({ agent: "hermes", available: true, supported: false })).toBe("installed (unsupported)");
    expect(agentRuntimeLabel("hermes", {
      agent: "hermes",
      agent_base: "hermes",
      available: true,
      version: "0.20.0",
      supported: true,
      path: "/private/probe/output/hermes",
    })).toBe("Hermes — installed (supported)");
  });

  test("does not allow an unknown or unsupported runtime to launch", () => {
    expect(isLaunchableRuntime("hermes", {
      agent: "hermes",
      agent_base: "hermes",
      available: true,
      supported: true,
    })).toBe(true);
    expect(isLaunchableRuntime("future", {
      agent: "future",
      agent_base: "future-agent",
      available: true,
    })).toBe(false);
    expect(isLaunchableRuntime("hermes", {
      agent: "hermes",
      agent_base: "hermes",
      available: true,
      supported: false,
    })).toBe(false);
    expect(isLaunchableRuntime("hermes", {
      agent: "hermes",
      available: true,
    })).toBe(false);
    expect(isLaunchableRuntime("hermes-work", {
      agent: "hermes-work",
      agent_base: "hermes",
      available: true,
    })).toBe(false);
  });

  test("preserves legacy profiles without inferring their name as an agent kind", () => {
    const legacyProfile = { agent: "review-profile", available: true } as const;
    expect(agentRuntimeLabel("review-profile", legacyProfile)).toBe("review-profile — installed");
    expect(isLaunchableRuntime("review-profile", legacyProfile)).toBe(true);
    expect(isLaunchableRuntime("review-profile", {
      ...legacyProfile,
      agent_base: "future-agent",
    })).toBe(false);
  });

  test("requires persisted and active agent bases to be known before showing mutations", () => {
    expect(hasKnownSessionAgentBases("hermes")).toBe(true);
    expect(hasKnownSessionAgentBases("hermes", "codex")).toBe(true);
    expect(hasKnownSessionAgentBases("future-agent")).toBe(false);
    expect(hasKnownSessionAgentBases("hermes", "future-agent")).toBe(false);
  });

  test("fails attach closed only for explicitly unknown session bases", () => {
    for (const base of ["shell", "codex", "claude", "hermes"] as const) {
      expect(hasAttachableSessionAgentBases(base)).toBe(true);
    }
    expect(hasAttachableSessionAgentBases("hermes", "shell")).toBe(true);
    expect(hasAttachableSessionAgentBases(undefined, undefined)).toBe(true);
    expect(hasAttachableSessionAgentBases("future-agent")).toBe(false);
    expect(hasAttachableSessionAgentBases("codex", "future-agent")).toBe(false);
    expect(sessionAgentLabel("legacy-profile", undefined)).toBe("legacy-profile");
  });
});
