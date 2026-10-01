import { posix } from "node:path";

/**
 * Daemon runtime-path contract shared with the Rust `pohunek-paths` crate.
 *
 * The cases in `crates/paths/fixtures/runtime-paths.json` drive both
 * implementations, so a change to either side fails the other's tests.
 */

export type RuntimePlatform = "linux" | "macos";

export const ENV_XDG_RUNTIME_DIR = "XDG_RUNTIME_DIR";

/** Application directory under the runtime base directory. */
export const APP_DIR = "pohunek";
/** Control socket filename under the runtime directory. */
export const SOCKET_NAME = "daemon.sock";
/** Runtime directory prefix used on macOS when `XDG_RUNTIME_DIR` is absent. */
export const MACOS_DEFAULT_RUNTIME_PREFIX = "/private/tmp/pohunek-";

/**
 * `sockaddr_un.sun_path` capacity excluding the native terminator. Darwin's
 * path is 104 bytes and Linux's 108, which is why a macOS runtime root must stay short.
 */
export const SOCKET_PATH_MAX_BYTES: Readonly<Record<RuntimePlatform, number>> = {
  linux: 107,
  macos: 103,
};

export type RuntimePathReason = "empty" | "not_absolute" | "parent_component" | "contains_nul";

export type RuntimePathFailure =
  | { readonly variant: "missing_env"; readonly variable: string }
  | { readonly variant: "invalid_env"; readonly variable: string; readonly reason: RuntimePathReason }
  | { readonly variant: "socket_path_invalid"; readonly variable: string; readonly detail: string };

const REASON_TEXT: Readonly<Record<RuntimePathReason, string>> = {
  empty: "must not be empty when present",
  not_absolute: "must be an absolute path",
  parent_component: "must not contain a parent-directory component",
  contains_nul: "must not contain a NUL byte",
};

export class RuntimePathError extends Error {
  public override readonly name = "RuntimePathError";
  public readonly failure: RuntimePathFailure;

  public constructor(failure: RuntimePathFailure) {
    super(runtimePathMessage(failure));
    this.failure = failure;
  }

  public get variable(): string {
    return this.failure.variable;
  }
}

function runtimePathMessage(failure: RuntimePathFailure): string {
  switch (failure.variant) {
    case "missing_env":
      return "is required (no safe default exists on this platform)";
    case "invalid_env":
      return REASON_TEXT[failure.reason];
    case "socket_path_invalid":
      return failure.detail;
  }
}

export interface RuntimePathContext {
  readonly platform: RuntimePlatform;
  readonly effectiveUid: number;
}

/** Selects the contract platform of this process; Windows and other hosts are unsupported. */
export function currentRuntimePathContext(): RuntimePathContext {
  const platform = process.platform;
  if (platform !== "linux" && platform !== "darwin") {
    throw new Error(`runtime path resolution is unsupported on platform ${platform}`);
  }
  if (typeof process.geteuid !== "function") {
    throw new Error("the effective user id is unavailable on this platform");
  }
  return {
    platform: platform === "darwin" ? "macos" : "linux",
    effectiveUid: process.geteuid(),
  };
}

/** Resolves the daemon runtime directory (`<base>/pohunek`, or the macOS owner default). */
export function resolveRuntimeDir(
  context: RuntimePathContext,
  env: Readonly<Record<string, string | undefined>>,
): string {
  const explicit = env[ENV_XDG_RUNTIME_DIR];
  if (explicit !== undefined) {
    return posix.join(validateBasePath(explicit, ENV_XDG_RUNTIME_DIR), APP_DIR);
  }
  if (context.platform === "macos") {
    return `${MACOS_DEFAULT_RUNTIME_PREFIX}${context.effectiveUid}`;
  }
  throw new RuntimePathError({ variant: "missing_env", variable: ENV_XDG_RUNTIME_DIR });
}

/** Resolves the daemon control socket path and checks it against the native limit. */
export function resolveDaemonSocket(
  context: RuntimePathContext,
  env: Readonly<Record<string, string | undefined>>,
): string {
  const socket = posix.join(resolveRuntimeDir(context, env), SOCKET_NAME);
  validateSocketPath(context.platform, socket, ENV_XDG_RUNTIME_DIR);
  return socket;
}

/**
 * Checks an explicitly configured socket path with the same rules as a
 * resolved one: absolute, normalized of parent components, NUL free, and
 * within the platform's `sun_path` capacity.
 */
export function validateSocketPath(
  platform: RuntimePlatform,
  socket: string,
  variable: string,
): void {
  if (socket.includes("\0")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "contains_nul" });
  }
  if (!socket.startsWith("/")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "not_absolute" });
  }
  if (socket.split("/").includes("..")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "parent_component" });
  }
  const bytes = Buffer.byteLength(socket);
  const limit = SOCKET_PATH_MAX_BYTES[platform];
  if (bytes > limit) {
    throw new RuntimePathError({
      variant: "socket_path_invalid",
      variable,
      detail: `resolves to a ${bytes}-byte socket path but ${platform} permits at most ${limit} bytes: ${socket}`,
    });
  }
}

function validateBasePath(value: string, variable: string): string {
  if (value.length === 0) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "empty" });
  }
  if (value.includes("\0")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "contains_nul" });
  }
  if (!value.startsWith("/")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "not_absolute" });
  }
  if (value.split("/").includes("..")) {
    throw new RuntimePathError({ variant: "invalid_env", variable, reason: "parent_component" });
  }
  return value;
}
