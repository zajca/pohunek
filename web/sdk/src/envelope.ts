import type { ProtocolError, ProtocolVersion, ProtocolVersionRange } from "@pohunek/protocol";
import { hasValidWireOrigin } from "./origin";

export interface Request {
  v: ProtocolVersionRange;
  id: string;
  method: string;
  params: unknown;
  origin_session_id?: string;
  origin_daemon_id?: string;
}

export interface OkResponse {
  v: ProtocolVersion;
  id: string;
  ok: unknown;
}

export interface ErrResponse {
  v: ProtocolVersion;
  id: string;
  err: ProtocolError;
}

export type Response = OkResponse | ErrResponse;

export type Event = {
  v: ProtocolVersion;
  event: string;
  id?: string;
} & Record<string, unknown>;

export function isRequest(value: unknown): value is Request {
  if (!isRecord(value)) {
    return false;
  }
  return (
    isProtocolVersionRange(value["v"]) &&
    typeof value["id"] === "string" &&
    typeof value["method"] === "string" &&
    hasOwn(value, "params") &&
    hasValidWireOrigin(value["origin_session_id"], value["origin_daemon_id"])
  );
}

export function isProtocolVersionRange(value: unknown): value is ProtocolVersionRange {
  if (!isRecord(value)) {
    return false;
  }
  const keys = Object.keys(value);
  const minimum = value["minimum"];
  const maximum = value["maximum"];
  return (
    keys.length === 2
    && keys.includes("minimum")
    && keys.includes("maximum")
    && isProtocolVersion(minimum)
    && isProtocolVersion(maximum)
    && minimum <= maximum
  );
}

export function isOkResponse(value: unknown): value is OkResponse {
  if (!isRecord(value)) {
    return false;
  }
  return (
    isProtocolVersion(value["v"]) &&
    typeof value["id"] === "string" &&
    hasOwn(value, "ok") &&
    !hasOwn(value, "err")
  );
}

export function isErrResponse(value: unknown): value is ErrResponse {
  if (!isRecord(value)) {
    return false;
  }
  return (
    isProtocolVersion(value["v"]) &&
    typeof value["id"] === "string" &&
    !hasOwn(value, "ok") &&
    isProtocolError(value["err"])
  );
}

export function decodeResponse(value: unknown): Response {
  if (isOkResponse(value)) {
    return value;
  }
  if (isErrResponse(value)) {
    return value;
  }
  throw new Error("invalid response envelope");
}

export function isEvent(value: unknown): value is Event {
  if (!isRecord(value)) {
    return false;
  }
  const id = value["id"];
  return (
    isProtocolVersion(value["v"]) &&
    typeof value["event"] === "string" &&
    (id === undefined || typeof id === "string")
  );
}

function isProtocolVersion(value: unknown): value is ProtocolVersion {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function isProtocolError(value: unknown): value is ProtocolError {
  if (!isRecord(value)) {
    return false;
  }
  const recover = value["recover"];
  return (
    isErrorClass(value["class"]) &&
    typeof value["code"] === "string" &&
    typeof value["msg"] === "string" &&
    (recover === undefined || typeof recover === "string")
  );
}

function isErrorClass(value: unknown): value is ProtocolError["class"] {
  return (
    value === "configuration" ||
    value === "daemon" ||
    value === "transport" ||
    value === "runtime" ||
    value === "discovery"
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOwn(value: Record<string, unknown>, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(value, key);
}
