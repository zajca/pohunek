import {
  EVENT_AGENT_STATE,
  EVENT_SESSION_CREATED,
  EVENT_SESSION_NATIVE_RECOVERED,
  EVENT_SESSION_REMOVED,
  EVENT_SESSION_RUNTIME_CONFLICT,
  EVENT_SESSION_RUNTIME_DISCOVERED,
  EVENT_SESSION_RUNTIME_LOST,
  EVENT_SESSION_RUNTIME_RECONNECTED,
  EVENT_SESSION_STOPPED,
  EVENT_SESSION_UPDATED,
  EVENT_SUBAGENT_STATE,
  MIN_PROTOCOL_VERSION,
  PREVIOUS_VERSION_INTRODUCED_METHODS,
  PREVIOUS_VERSION_RENAMED_KEYS,
  PROTOCOL_VERSION,
} from "@pohunek/protocol";

/**
 * Edge adapter between the current public protocol and the previous version.
 *
 * Mirrors `crates/protocol/src/compat/v3.rs` for the directions a client needs:
 * requests are translated down to the previous shape, results and events up to
 * the current one. The adapter translates shape only. The key names and the list
 * of methods the previous version never defined are generated from the Rust
 * adapter; the sites that carry a renamed key are checked against the golden
 * payloads recorded from the previous release (`test/previous-daemon.test.ts`).
 */

/** Protocol version this adapter serves. */
export const ADAPTED_VERSION = 3;

const LEGACY_KEY = renamedKey(0, "runtime_id");
const CURRENT_KEY = renamedKey(1, "worker_instance_id");
const LEGACY_PREVIOUS_KEY = renamedKey(2, "previous_runtime_id");
const CURRENT_PREVIOUS_KEY = renamedKey(3, "previous_worker_instance_id");

function renamedKey(index: number, expected: string): string {
  const pair = PREVIOUS_VERSION_RENAMED_KEYS[Math.floor(index / 2)];
  const key = pair?.[index % 2];
  if (key !== expected) {
    throw new Error(`generated protocol key mapping changed (${expected}); update sdk/src/compat.ts`);
  }
  return key;
}

const SESSION_EVENTS: readonly string[] = [
  EVENT_SESSION_CREATED,
  EVENT_SESSION_UPDATED,
  EVENT_SESSION_STOPPED,
  EVENT_SESSION_REMOVED,
  EVENT_SESSION_RUNTIME_RECONNECTED,
  EVENT_SESSION_RUNTIME_LOST,
  EVENT_SESSION_RUNTIME_CONFLICT,
];

/** Why the adapter could not translate a payload. */
export type CompatFailure =
  | { kind: "unsupportedVersion"; version: number }
  | { kind: "conflict"; site: string; key: string }
  | { kind: "methodNotDefined"; method: string };

/** Raised for a request, result or event the adapter cannot translate. */
export class CompatError extends Error {
  public override readonly name = "CompatError";
  public readonly failure: CompatFailure;

  public constructor(failure: CompatFailure) {
    super(describe(failure));
    this.failure = failure;
  }
}

function describe(failure: CompatFailure): string {
  switch (failure.kind) {
    case "unsupportedVersion":
      return `public protocol version ${failure.version} has no edge adapter`;
    case "conflict":
      return `\`${failure.key}\` already exists at ${failure.site} and cannot be translated`;
    case "methodNotDefined":
      return `method \`${failure.method}\` is not defined in this protocol version`;
  }
}

type Direction = "toCurrent" | "toLegacy";

function keys(direction: Direction): [string, string] {
  return direction === "toCurrent" ? [LEGACY_KEY, CURRENT_KEY] : [CURRENT_KEY, LEGACY_KEY];
}

function previousKeys(direction: Direction): [string, string] {
  return direction === "toCurrent"
    ? [LEGACY_PREVIOUS_KEY, CURRENT_PREVIOUS_KEY]
    : [CURRENT_PREVIOUS_KEY, LEGACY_PREVIOUS_KEY];
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function adapted(version: number): "current" | "v3" {
  if (version === PROTOCOL_VERSION) {
    return "current";
  }
  if (version === ADAPTED_VERSION && version === MIN_PROTOCOL_VERSION) {
    return "v3";
  }
  throw new CompatError({ kind: "unsupportedVersion", version });
}

function rejectIntroduced(method: string): void {
  if ((PREVIOUS_VERSION_INTRODUCED_METHODS as readonly string[]).includes(method)) {
    throw new CompatError({ kind: "methodNotDefined", method });
  }
}

function renameKey(object: Record<string, unknown>, from: string, to: string, site: string): void {
  if (Object.prototype.hasOwnProperty.call(object, to)) {
    throw new CompatError({ kind: "conflict", site, key: to });
  }
  if (Object.prototype.hasOwnProperty.call(object, from)) {
    object[to] = object[from];
    delete object[from];
  }
}

/** Renames the worker instance key of `value` when it is an object. */
function renameIn(value: unknown, site: string, direction: Direction): void {
  if (isObject(value)) {
    const [from, to] = keys(direction);
    renameKey(value, from, to, site);
  }
}

function nestedIdentity(container: unknown, site: string, direction: Direction): void {
  if (isObject(container)) {
    renameIn(container["runtime"], site, direction);
  }
}

function sessionInfo(session: unknown, site: string, direction: Direction): void {
  if (isObject(session)) {
    renameIn(session["runtime"], site, direction);
  }
}

function field(value: unknown, key: string): unknown {
  return isObject(value) ? value[key] : undefined;
}

function copy<T>(value: T): T {
  return structuredClone(value);
}

/** Translates current request parameters into the shape of `version`. */
export function downgradeRequestParams(version: number, method: string, params: unknown): unknown {
  if (adapted(version) === "current") {
    return params;
  }
  rejectIntroduced(method);
  const translated = copy(params);
  switch (method) {
    case "session.output":
    case "session.wait":
      nestedIdentity(translated, "params.runtime", "toLegacy");
      break;
    case "session.report_native_id":
      renameIn(translated, "params", "toLegacy");
      break;
    default:
      break;
  }
  return translated;
}

/** Translates a success payload of `version` into the current shape. */
export function upgradeResult(version: number, method: string, result: unknown): unknown {
  if (adapted(version) === "current") {
    return result;
  }
  rejectIntroduced(method);
  const translated = copy(result);
  switch (method) {
    case "session.list":
      if (Array.isArray(translated)) {
        for (const session of translated) {
          sessionInfo(session, "result[]", "toCurrent");
        }
      }
      break;
    // `session.new` and `session.fork` flatten the session into the result.
    case "session.inspect":
    case "session.new":
    case "session.fork":
      sessionInfo(translated, "result", "toCurrent");
      break;
    case "session.resume":
    case "session.resize":
    case "session.set_metadata":
    case "session.rename":
    case "session.wait":
      sessionInfo(field(translated, "session"), "result.session", "toCurrent");
      break;
    case "session.input":
      nestedIdentity(translated, "result.runtime", "toCurrent");
      break;
    case "session.screen":
    case "session.read":
    case "session.output":
      renameIn(translated, "result", "toCurrent");
      break;
    case "session.runtime_inventory": {
      const entries = field(translated, "entries");
      if (Array.isArray(entries)) {
        for (const entry of entries) {
          renameIn(entry, "result.entries[]", "toCurrent");
        }
      }
      break;
    }
    default:
      break;
  }
  return translated;
}

/**
 * Translates an event payload of `version` into the current shape.
 *
 * An event outside the renamed sites passes through untouched.
 */
export function upgradeEventPayload(version: number, name: string, payload: unknown): unknown {
  if (adapted(version) === "current") {
    return payload;
  }
  const translated = copy(payload);
  const direction: Direction = "toCurrent";
  if (name === EVENT_AGENT_STATE || name === EVENT_SUBAGENT_STATE) {
    nestedIdentity(translated, "payload.runtime", direction);
  } else if (name === EVENT_SESSION_RUNTIME_DISCOVERED) {
    renameIn(field(translated, "entry"), "payload.entry", direction);
  } else if (name === EVENT_SESSION_NATIVE_RECOVERED) {
    sessionInfo(field(translated, "session"), "payload.session", direction);
    if (isObject(translated)) {
      const [previousFrom, previousTo] = previousKeys(direction);
      renameKey(translated, previousFrom, previousTo, "payload");
      const [from, to] = keys(direction);
      renameKey(translated, from, to, "payload");
    }
  } else if (SESSION_EVENTS.includes(name)) {
    sessionInfo(field(translated, "session"), "payload.session", direction);
  }
  return translated;
}

/**
 * Reports whether a current request means the same in every older version.
 *
 * A client that has not selected a version yet can send such a request without
 * learning the daemon's version first.
 */
export function requestIsVersionIndependent(method: string, params: unknown): boolean {
  for (let version = MIN_PROTOCOL_VERSION; version < PROTOCOL_VERSION; version += 1) {
    try {
      const downgraded = downgradeRequestParams(version, method, params);
      if (JSON.stringify(downgraded) !== JSON.stringify(params)) {
        return false;
      }
    } catch (error: unknown) {
      if (error instanceof CompatError) {
        return false;
      }
      throw error;
    }
  }
  return true;
}
