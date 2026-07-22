import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const DEFAULT_REMOTE_PORT = 18_722;
export const DEFAULT_DISCOVER_INTERVAL_SECONDS = 30;
export const DEFAULT_STATIC_ASSETS_DIR = fileURLToPath(
  new URL("../../frontend/dist", import.meta.url),
);

export const ENV_BIND_HOST = "POHUNEK_BACKEND_BIND_HOST";
export const ENV_PORT = "POHUNEK_BACKEND_PORT";
export const ENV_ALLOW_LOOPBACK = "POHUNEK_BACKEND_ALLOW_LOOPBACK";
export const ENV_DAEMON_SOCKET = "POHUNEK_BACKEND_DAEMON_SOCKET";
export const ENV_DISCOVER_INTERVAL = "POHUNEK_BACKEND_DISCOVER_INTERVAL";
export const ENV_STATIC_ASSETS_DIR = "POHUNEK_BACKEND_STATIC_DIR";
export const ENV_REMOTE_PORT = "POHUNEK_REMOTE_PORT";
export const ENV_XDG_RUNTIME_DIR = "XDG_RUNTIME_DIR";

const DEFAULT_DAEMON_SOCKET_SUBDIRECTORY = "pohunek";
const DEFAULT_DAEMON_SOCKET_FILENAME = "daemon.sock";
const MIN_PORT = 0;
const MIN_REMOTE_PORT = 1;
const MAX_PORT = 65_535;
const TRUE_ENV_VALUES = new Set(["1", "true", "yes", "on"]);
const FALSE_ENV_VALUES = new Set(["0", "false", "no", "off"]);

export interface BackendConfig {
  readonly bindHost: string;
  readonly port: number;
  readonly allowLoopbackBind: boolean;
  readonly daemonSocketPath: string;
  readonly remotePort: number;
  readonly discoverIntervalSeconds: number;
  readonly staticAssetsDir: string;
}

export class BackendConfigError extends Error {
  public override readonly name = "BackendConfigError";
  public readonly variable: string;

  public constructor(variable: string, message: string) {
    super(`${variable} ${message}`);
    this.variable = variable;
  }
}

export function loadBackendConfig(env: NodeJS.ProcessEnv = process.env): BackendConfig {
  return {
    bindHost: requiredEnv(env, ENV_BIND_HOST),
    port: parsePort(requiredEnv(env, ENV_PORT), ENV_PORT, MIN_PORT),
    allowLoopbackBind: parseBoolean(env[ENV_ALLOW_LOOPBACK], ENV_ALLOW_LOOPBACK),
    daemonSocketPath: resolveDaemonSocketPath(env),
    remotePort: parseOptionalPort(env[ENV_REMOTE_PORT]),
    discoverIntervalSeconds: parseDiscoverInterval(env[ENV_DISCOVER_INTERVAL]),
    staticAssetsDir: resolveStaticAssetsDir(env[ENV_STATIC_ASSETS_DIR]),
  };
}

function requiredEnv(env: NodeJS.ProcessEnv, name: string): string {
  const value = env[name];
  if (value === undefined || value.length === 0) {
    throw new BackendConfigError(name, "is required and must not be empty");
  }
  return value;
}

function resolveDaemonSocketPath(env: NodeJS.ProcessEnv): string {
  const override = env[ENV_DAEMON_SOCKET];
  if (override !== undefined) {
    if (override.length === 0) {
      throw new BackendConfigError(ENV_DAEMON_SOCKET, "must not be empty when present");
    }
    return override;
  }

  const runtimeDir = env[ENV_XDG_RUNTIME_DIR];
  if (runtimeDir === undefined || runtimeDir.length === 0) {
    throw new BackendConfigError(
      ENV_XDG_RUNTIME_DIR,
      `is required when ${ENV_DAEMON_SOCKET} is not set`,
    );
  }
  return join(runtimeDir, DEFAULT_DAEMON_SOCKET_SUBDIRECTORY, DEFAULT_DAEMON_SOCKET_FILENAME);
}

function parseOptionalPort(raw: string | undefined): number {
  if (raw === undefined) {
    return DEFAULT_REMOTE_PORT;
  }
  return parsePort(raw, ENV_REMOTE_PORT, MIN_REMOTE_PORT);
}

function parsePort(raw: string, variable: string, minimum: number): number {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    throw new BackendConfigError(variable, `must be an integer port from ${minimum} to ${MAX_PORT}`);
  }
  const value = Number(trimmed);
  if (!Number.isInteger(value) || value < minimum || value > MAX_PORT) {
    throw new BackendConfigError(variable, `must be an integer port from ${minimum} to ${MAX_PORT}`);
  }
  return value;
}

function parseBoolean(raw: string | undefined, variable: string): boolean {
  if (raw === undefined) {
    return false;
  }
  const normalized = raw.trim().toLowerCase();
  if (TRUE_ENV_VALUES.has(normalized)) {
    return true;
  }
  if (FALSE_ENV_VALUES.has(normalized)) {
    return false;
  }
  throw new BackendConfigError(variable, "must be one of true/false, yes/no, on/off, or 1/0");
}

function parseDiscoverInterval(raw: string | undefined): number {
  if (raw === undefined) {
    return DEFAULT_DISCOVER_INTERVAL_SECONDS;
  }
  const value = Number(raw.trim());
  if (!Number.isFinite(value) || value <= 0) {
    throw new BackendConfigError(ENV_DISCOVER_INTERVAL, "must be a positive number of seconds");
  }
  return value;
}

function resolveStaticAssetsDir(raw: string | undefined): string {
  if (raw === undefined) {
    return DEFAULT_STATIC_ASSETS_DIR;
  }
  if (raw.length === 0) {
    throw new BackendConfigError(ENV_STATIC_ASSETS_DIR, "must not be empty when present");
  }
  return resolve(raw);
}
