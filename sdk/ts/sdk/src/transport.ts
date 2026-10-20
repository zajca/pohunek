import { resolveRequestOrigin, type RequestOrigin } from "./origin";

export interface ConnectOptions {
  connectTimeoutMs?: number;
  requestTimeoutMs?: number;
  /** Explicit origin for a client hosted inside one managed session. */
  origin?: RequestOrigin;
}

export interface ResolvedConnectOptions {
  connectTimeoutMs: number;
  requestTimeoutMs: number;
  /** Present when the caller chose the response deadline explicitly. */
  explicitRequestTimeoutMs?: number;
  origin?: RequestOrigin;
}

export interface ControlChannel {
  send(line: string): Promise<void>;
  readonly lines: AsyncIterable<string>;
  close(): Promise<void>;
}

export interface RawDuplex {
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
  close(): Promise<void>;
}

export interface Transport {
  control(): Promise<ControlChannel>;
  raw(): Promise<RawDuplex>;
}

export const DEFAULT_CONNECT_TIMEOUT_MS = 5_000;
export const DEFAULT_REQUEST_TIMEOUT_MS = 5_000;
/** Budget for `session.new`: three 10-minute native service ceilings (worker
 * connection, initialization and, with initial input, reader grace), the
 * `pre-create` and `post-create` hooks with a host and a repository layer of
 * 5 minutes each, and 10 minutes for the rest of creation. */
export const SESSION_NEW_REQUEST_BUDGET_MS = 60 * 60 * 1_000;
/** Longest valid daemon bound of one lifecycle step: worker connection, worker
 * initialization, a job's exit timeout and the marker sweep's grace are each
 * validated up to 10 minutes. A shared fixture ties it to the Rust client. */
export const LIFECYCLE_STEP_BOUND_MS = 10 * 60 * 1_000;
/** Signalling passes the daemon's marker sweep makes at most. */
export const RESUME_SWEEP_PASSES = 3;
/** Steps of the daemon's sweep: each pass waits a `SIGTERM` grace and a
 * `SIGKILL` grace, plus one settle window. */
const SWEEP_STEPS = 2 * RESUME_SWEEP_PASSES + 1;
/** Resume: retiring the lost generation's job (1), the cleanup sweep, the new
 * worker's connection and initialization (2), and the rest of the resume (1). */
const SESSION_RESUME_STEPS = 1 + SWEEP_STEPS + 2 + 1;
/** Fork: the new worker's connection and initialization (2) and the rest of
 * the fork (1); a fork runs no create hooks. */
const SESSION_FORK_STEPS = 2 + 1;
/** Stop and remove: the worker's stop or a conflicted runtime's job
 * retirement (1), the conflicted runtime's cleanup sweep, and the rest (1). */
const SESSION_STOP_STEPS = 1 + SWEEP_STEPS + 1;
/** Budget for `session.resume`. */
export const SESSION_RESUME_REQUEST_BUDGET_MS = SESSION_RESUME_STEPS * LIFECYCLE_STEP_BOUND_MS;
/** Budget for `session.fork`. */
export const SESSION_FORK_REQUEST_BUDGET_MS = SESSION_FORK_STEPS * LIFECYCLE_STEP_BOUND_MS;
/** Budget for `session.stop`. */
export const SESSION_STOP_REQUEST_BUDGET_MS = SESSION_STOP_STEPS * LIFECYCLE_STEP_BOUND_MS;
/** Longest one worktree hook layer runs; a shared fixture ties it to the
 * Rust client, whose daemon test ties it to the daemon's hook timeout. */
export const WORKTREE_HOOK_TIMEOUT_MS = 5 * 60 * 1_000;
/** Hook runs a removal waits for: `pre-remove` and `post-remove`, each with a
 * host layer and a repository layer. */
const SESSION_REMOVE_HOOK_RUNS = 2 * 2;
/** Budget for `session.remove` and `session.remove_accepting_unconfirmed`: the
 * stop the removal performs first plus every hook run at its timeout. */
export const SESSION_REMOVE_REQUEST_BUDGET_MS =
  SESSION_STOP_REQUEST_BUDGET_MS + SESSION_REMOVE_HOOK_RUNS * WORKTREE_HOOK_TIMEOUT_MS;

export function resolveConnectOptions(options: ConnectOptions = {}): ResolvedConnectOptions {
  return {
    connectTimeoutMs: options.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    requestTimeoutMs: options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS,
    ...(options.requestTimeoutMs === undefined ? {} : { explicitRequestTimeoutMs: options.requestTimeoutMs }),
    ...(options.origin === undefined ? {} : { origin: resolveRequestOrigin(options.origin) }),
  };
}
