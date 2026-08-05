/** Origin of a client running inside one Pohunek-managed session. */
export interface RequestOrigin {
  sessionId: string;
  daemonId: string;
}

// Mirrors the public envelope's session/origin identifier ceiling. Both origin
// coordinates use the same bound so validation stays atomic and predictable.
const MAX_REQUEST_ORIGIN_ID_BYTES = 128;
const SAFE_ORIGIN_ID = /^[A-Za-z0-9._:-]+$/;

/** Returns an immutable validated request origin. */
export function resolveRequestOrigin(origin: RequestOrigin): RequestOrigin {
  if (
    typeof origin !== "object"
    || origin === null
    || !isValidOriginIdentifier(origin.sessionId)
    || !isValidOriginIdentifier(origin.daemonId)
  ) {
    throw new TypeError(
      "request origin requires paired non-empty ASCII identifiers of at most 128 bytes",
    );
  }
  return Object.freeze({ sessionId: origin.sessionId, daemonId: origin.daemonId });
}

/** Reports whether optional wire fields form one valid atomic origin pair. */
export function hasValidWireOrigin(
  sessionId: unknown,
  daemonId: unknown,
): boolean {
  if (sessionId === undefined && daemonId === undefined) {
    return true;
  }
  return (
    typeof sessionId === "string"
    && typeof daemonId === "string"
    && isValidOriginIdentifier(sessionId)
    && isValidOriginIdentifier(daemonId)
  );
}

function isValidOriginIdentifier(value: unknown): value is string {
  return (
    typeof value === "string"
    && value.length > 0
    && value.length <= MAX_REQUEST_ORIGIN_ID_BYTES
    && SAFE_ORIGIN_ID.test(value)
  );
}
