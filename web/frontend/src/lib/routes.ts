export type AppRoute =
  | { readonly kind: "workspace" }
  | { readonly kind: "new-session"; readonly host?: string }
  | { readonly kind: "session"; readonly host: string; readonly sessionId: string }
  | { readonly kind: "terminal"; readonly host: string; readonly sessionId: string }
  | { readonly kind: "inbox" };

export type RouteOverlay = "new-session" | "session-inspector" | "inbox";

export interface RouteSessionSelection {
  readonly host: string;
  readonly sessionId: string;
}

const HOSTS_SEGMENT = "hosts";
const SESSIONS_SEGMENT = "sessions";
const NEW_SEGMENT = "new";
const TERMINAL_SEGMENT = "terminal";
const INBOX_SEGMENT = "inbox";

export function routePath(route: AppRoute): string {
  switch (route.kind) {
    case "workspace":
      return "/";
    case "new-session":
      return route.host === undefined
        ? `/${SESSIONS_SEGMENT}/${NEW_SEGMENT}`
        : `/${HOSTS_SEGMENT}/${encodeRouteSegment(route.host)}/${SESSIONS_SEGMENT}/${NEW_SEGMENT}`;
    case "session":
      return sessionPath(route.host, route.sessionId);
    case "terminal":
      return `${sessionPath(route.host, route.sessionId)}/${TERMINAL_SEGMENT}`;
    case "inbox":
      return `/${INBOX_SEGMENT}`;
  }
}

/** Parses an application pathname, returning undefined for malformed or unknown routes. */
export function parseRoute(pathname: string): AppRoute | undefined {
  if (pathname === "/" || pathname.length === 0) {
    return { kind: "workspace" };
  }

  const rawSegments = pathname.split("/").filter((segment) => segment.length > 0);
  if (rawSegments.length === 1 && rawSegments[0] === INBOX_SEGMENT) {
    return { kind: "inbox" };
  }
  if (
    rawSegments.length === 2
    && rawSegments[0] === SESSIONS_SEGMENT
    && rawSegments[1] === NEW_SEGMENT
  ) {
    return { kind: "new-session" };
  }

  if (
    rawSegments.length < 4
    || rawSegments[0] !== HOSTS_SEGMENT
    || rawSegments[2] !== SESSIONS_SEGMENT
  ) {
    return undefined;
  }

  const host = decodeRouteSegment(rawSegments[1]);
  if (host === undefined) {
    return undefined;
  }
  if (rawSegments.length === 4 && rawSegments[3] === NEW_SEGMENT) {
    return { kind: "new-session", host };
  }

  const sessionId = decodeRouteSegment(rawSegments[3]);
  if (sessionId === undefined) {
    return undefined;
  }
  if (rawSegments.length === 4) {
    return { kind: "session", host, sessionId };
  }
  if (rawSegments.length === 5 && rawSegments[4] === TERMINAL_SEGMENT) {
    return { kind: "terminal", host, sessionId };
  }
  return undefined;
}

export function encodeRouteSegment(value: string): string {
  if (value.length === 0) {
    throw new Error("route segments must not be empty");
  }
  return encodeURIComponent(value);
}

export function decodeRouteSegment(value: string | undefined): string | undefined {
  if (value === undefined || value.length === 0) {
    return undefined;
  }
  try {
    const decoded = decodeURIComponent(value);
    return decoded.length === 0 ? undefined : decoded;
  } catch {
    return undefined;
  }
}

/** Maps legacy page routes to the overlay shown by the persistent application shell. */
export function overlayForRoute(route: AppRoute): RouteOverlay | undefined {
  switch (route.kind) {
    case "new-session":
      return "new-session";
    case "session":
      return "session-inspector";
    case "inbox":
      return "inbox";
    case "workspace":
    case "terminal":
      return undefined;
  }
}

/** Extracts the session selected by either a detail or terminal deep link. */
export function sessionSelectionFromRoute(route: AppRoute): RouteSessionSelection | undefined {
  if (route.kind !== "session" && route.kind !== "terminal") {
    return undefined;
  }
  return { host: route.host, sessionId: route.sessionId };
}

function sessionPath(host: string, sessionId: string): string {
  return `/${HOSTS_SEGMENT}/${encodeRouteSegment(host)}/${SESSIONS_SEGMENT}/${encodeRouteSegment(sessionId)}`;
}
