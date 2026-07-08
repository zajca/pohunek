import type { ProtocolError, ProtocolVersion } from "@pohunek/protocol";

export interface Request {
  v: ProtocolVersion;
  id: string;
  method: string;
  params: unknown;
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
    typeof value["v"] === "number" &&
    typeof value["id"] === "string" &&
    typeof value["method"] === "string" &&
    hasOwn(value, "params")
  );
}

export function isOkResponse(value: unknown): value is OkResponse {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value["v"] === "number" &&
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
    typeof value["v"] === "number" &&
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
    typeof value["v"] === "number" &&
    typeof value["event"] === "string" &&
    (id === undefined || typeof id === "string")
  );
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
