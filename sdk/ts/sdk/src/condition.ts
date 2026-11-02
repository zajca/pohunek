import type { RuntimeState, SessionInfo, SessionState } from "@pohunek/protocol";

/**
 * The one user-facing condition of a session, derived on the client from
 * `SessionInfo`. The daemon never sends it; `crates/protocol` implements the
 * same rule as `SessionInfo::condition()`, and both must answer alike.
 */
export type SessionCondition =
  | SessionState
  | "crashed"
  | "unknown"
  | Extract<RuntimeState, "reconnecting" | "conflict" | "incompatible">;

/** How a `crashed` session can be brought back; `null` for every other condition. */
export type SessionRecovery =
  | { readonly kind: "no_native_resume" }
  | { readonly kind: "refused"; readonly code: string }
  | { readonly kind: "recoverable" }
  | { readonly kind: "unknown" };

/** Where the `SessionInfo` came from. Only `session.list` and `session.inspect` answer for a live daemon. */
export interface SessionConditionOptions {
  readonly listed?: boolean;
}

export interface SessionConditionResult {
  readonly condition: SessionCondition;
  readonly recovery: SessionRecovery | null;
}

/**
 * Every runtime state of the generated `RuntimeState` union. The record makes a
 * state added to or removed from the union a compile error here.
 */
const KNOWN_RUNTIME_STATES: Record<RuntimeState, true> = {
  starting: true,
  live: true,
  reconnecting: true,
  terminal: true,
  lost: true,
  conflict: true,
  incompatible: true,
};

const LIVE_SESSION_STATES: ReadonlySet<SessionState> = new Set<SessionState>(["starting", "running"]);

/**
 * Derives the condition of a session and, for `crashed` only, its recovery.
 * Rules are applied in order; the first match decides.
 */
export function sessionCondition(
  info: SessionInfo,
  options: SessionConditionOptions = {},
): SessionConditionResult {
  const condition = conditionOf(info);
  return {
    condition,
    recovery: condition === "crashed" ? recoveryOf(info, options) : null,
  };
}

function conditionOf(info: SessionInfo): SessionCondition {
  if (info.external === true) {
    return info.state;
  }
  const runtimeState = info.runtime?.state;
  // A lost runtime crashes the session whatever the session state says: the
  // daemon marks a failed session lost, and an unproven loss keeps it running.
  if (runtimeState === "lost") {
    return "crashed";
  }
  if (!LIVE_SESSION_STATES.has(info.state) || runtimeState === undefined) {
    return info.state;
  }
  if (runtimeState === "reconnecting" || runtimeState === "conflict" || runtimeState === "incompatible") {
    return runtimeState;
  }
  // A runtime state this build does not know may still be live, so it must not read as crashed.
  if (!isKnownRuntimeState(runtimeState)) {
    return "unknown";
  }
  return info.state;
}

function recoveryOf(info: SessionInfo, options: SessionConditionOptions): SessionRecovery {
  if (!info.capabilities.resume) {
    return { kind: "no_native_resume" };
  }
  const refusal = info.capability_refusals?.resume;
  if (typeof refusal === "string") {
    return { kind: "refused", code: refusal };
  }
  // Event payloads carry no refusals, so only a listed session proves recoverability.
  if (options.listed === true) {
    return { kind: "recoverable" };
  }
  return { kind: "unknown" };
}

function isKnownRuntimeState(value: string): value is RuntimeState {
  return Object.hasOwn(KNOWN_RUNTIME_STATES, value);
}
