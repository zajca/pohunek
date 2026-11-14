import { PROTOCOL_VERSION } from "@pohunek/protocol";

/**
 * Edge adapter between the public protocol and the version a daemon speaks.
 *
 * The protocol window is the single current version, so every payload passes
 * through unchanged and any other version is refused.
 */

/** Why the adapter could not translate a payload. */
export type CompatFailure =
  | { kind: "unsupportedVersion"; version: number }
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
    case "methodNotDefined":
      return `method \`${failure.method}\` is not defined in this protocol version`;
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isSet(value: unknown): boolean {
  return value !== undefined && value !== null;
}

/**
 * Whether `params` holds a non-empty `secret_env`, or a value the daemon must
 * judge. An empty object counts as absent.
 */
export function carriesSecretEnv(params: unknown): boolean {
  if (!isObject(params) || !("secret_env" in params)) {
    return false;
  }
  const value = params["secret_env"];
  if (value === undefined) {
    return false;
  }
  return !(isObject(value) && !Array.isArray(value) && Object.keys(value).length === 0);
}

/** Whether `params` carries a `client_request_id` the daemon must honor. */
export function carriesClientRequestId(params: unknown): boolean {
  return isObject(params) && isSet(params["client_request_id"]);
}

/** Whether `params` carries a `context_package_digest` the daemon must pin. */
export function carriesContextPackageDigest(params: unknown): boolean {
  return isObject(params) && isSet(params["context_package_digest"]);
}

/** The reserved metadata key whose single accepted value isolates descendants. */
export const ATTENTION_CHILDREN_METADATA_KEY = "pohunek.attention.children";

/** Whether `params` carries a `fetch_ref` the daemon must fetch and pin. */
export function carriesFetchRef(params: unknown): boolean {
  return isObject(params) && isSet(params["fetch_ref"]);
}

/**
 * Whether `session.new` params set the reserved attention flag. Clearing is
 * not a thing on `session.new`: its metadata holds plain string values.
 */
export function carriesAttentionIsolationFlag(params: unknown): boolean {
  return (
    isObject(params) &&
    isObject(params["metadata"]) &&
    params["metadata"] !== null &&
    ATTENTION_CHILDREN_METADATA_KEY in params["metadata"]
  );
}

/**
 * Whether a `session.set_metadata` patch sets the reserved attention flag.
 * Clearing the key (`null`) always passes through; only a value counts.
 */
export function setsAttentionIsolationFlag(params: unknown): boolean {
  if (!isObject(params) || !isObject(params["metadata"])) {
    return false;
  }
  const metadata = params["metadata"];
  return (
    ATTENTION_CHILDREN_METADATA_KEY in metadata && metadata[ATTENTION_CHILDREN_METADATA_KEY] !== null
  );
}

function requireCurrent(version: number): void {
  if (version !== PROTOCOL_VERSION) {
    throw new CompatError({ kind: "unsupportedVersion", version });
  }
}

/** Returns request parameters in the shape of `version`. */
export function downgradeRequestParams(version: number, _method: string, params: unknown): unknown {
  requireCurrent(version);
  return params;
}

/** Returns a success payload of `version` in the current shape. */
export function upgradeResult(version: number, _method: string, result: unknown): unknown {
  requireCurrent(version);
  return result;
}

/** Returns an event payload of `version` in the current shape. */
export function upgradeEventPayload(version: number, _name: string, payload: unknown): unknown {
  requireCurrent(version);
  return payload;
}

/**
 * Reports whether a current request means the same in every other version.
 *
 * The window holds one version, so a request is always version independent.
 */
export function requestIsVersionIndependent(): boolean {
  return true;
}
