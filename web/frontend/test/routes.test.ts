import { describe, expect, test } from "bun:test";
import {
  decodeRouteSegment,
  encodeRouteSegment,
  parseRoute,
  overlayForRoute,
  routePath,
  sessionSelectionFromRoute,
  type AppRoute,
} from "../src/lib/routes";

describe("frontend routes", () => {
  test("round-trips every application route", () => {
    const routes: readonly AppRoute[] = [
      { kind: "workspace" },
      { kind: "new-session" },
      { kind: "new-session", host: "local host" },
      { kind: "session", host: "dev/peer", sessionId: "session #1" },
      { kind: "terminal", host: "dev/peer", sessionId: "session #1" },
      { kind: "inbox" },
    ];

    for (const route of routes) {
      expect(parseRoute(routePath(route))).toEqual(route);
    }
  });

  test("encodes reserved characters inside one path segment", () => {
    const value = "host/name ?#%";
    const encoded = encodeRouteSegment(value);

    expect(encoded).not.toContain("/");
    expect(decodeRouteSegment(encoded)).toBe(value);
  });

  test("rejects malformed and unknown paths", () => {
    expect(parseRoute("/hosts/%E0%A4%A/sessions/id")).toBeUndefined();
    expect(parseRoute("/hosts/host/sessions/id/unknown")).toBeUndefined();
    expect(parseRoute("/unknown")).toBeUndefined();
    expect(decodeRouteSegment("%E0%A4%A")).toBeUndefined();
    expect(() => encodeRouteSegment("")).toThrow("route segments must not be empty");
  });

  test("maps deep links to shell overlays and session selection", () => {
    expect(overlayForRoute({ kind: "new-session", host: "dev" })).toBe("new-session");
    expect(overlayForRoute({ kind: "session", host: "dev", sessionId: "s-1" })).toBe("session-inspector");
    expect(overlayForRoute({ kind: "inbox" })).toBe("inbox");
    expect(overlayForRoute({ kind: "terminal", host: "dev", sessionId: "s-1" })).toBeUndefined();
    expect(sessionSelectionFromRoute({ kind: "terminal", host: "dev", sessionId: "s-1" })).toEqual({
      host: "dev",
      sessionId: "s-1",
    });
    expect(sessionSelectionFromRoute({ kind: "workspace" })).toBeUndefined();
  });
});
