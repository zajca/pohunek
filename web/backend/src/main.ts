import { pathToFileURL } from "node:url";
import { startRelay, type DaemonTarget } from "./relay";

const ENV_BIND_HOST = "POHUNEK_RELAY_BIND_HOST";
const ENV_PORT = "POHUNEK_RELAY_PORT";
const ENV_TARGETS_JSON = "POHUNEK_RELAY_TARGETS_JSON";
const ENV_ALLOW_LOOPBACK = "POHUNEK_RELAY_ALLOW_LOOPBACK";
const TRUE_ENV_VALUES = new Set(["1", "true", "yes", "on"]);

export async function startRelayFromEnv(env: NodeJS.ProcessEnv = process.env): Promise<void> {
  const bindHost = requiredEnv(env, ENV_BIND_HOST);
  const port = parsePort(requiredEnv(env, ENV_PORT));
  const targets = parseTargets(requiredEnv(env, ENV_TARGETS_JSON));
  const allowLoopbackBind = parseBoolean(env[ENV_ALLOW_LOOPBACK]);
  const relay = await startRelay({ bindHost, port, targets, allowLoopbackBind });
  console.log(`pohunek relay listening on ${relay.url}`);
}

function requiredEnv(env: NodeJS.ProcessEnv, name: string): string {
  const value = env[name];
  if (value === undefined || value.length === 0) {
    throw new Error(`missing required environment variable ${name}`);
  }
  return value;
}

function parsePort(raw: string): number {
  const value = Number(raw);
  if (!Number.isInteger(value) || value < 0 || value > 65_535) {
    throw new Error(`${ENV_PORT} must be an integer port from 0 to 65535`);
  }
  return value;
}

function parseBoolean(raw: string | undefined): boolean {
  return raw !== undefined && TRUE_ENV_VALUES.has(raw.toLowerCase());
}

function parseTargets(raw: string): ReadonlyMap<string, DaemonTarget> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw) as unknown;
  } catch (error: unknown) {
    throw new Error(`${ENV_TARGETS_JSON} must be a JSON object: ${messageFromUnknown(error)}`);
  }

  if (!isRecord(parsed)) {
    throw new Error(`${ENV_TARGETS_JSON} must be a JSON object keyed by host`);
  }

  const targets = new Map<string, DaemonTarget>();
  for (const [host, value] of Object.entries(parsed)) {
    targets.set(host, parseTarget(host, value));
  }
  if (targets.size === 0) {
    throw new Error(`${ENV_TARGETS_JSON} must contain at least one target`);
  }
  return targets;
}

function parseTarget(host: string, value: unknown): DaemonTarget {
  if (!isRecord(value) || typeof value["kind"] !== "string") {
    throw new Error(`target ${host} must be an object with a kind`);
  }

  if (value["kind"] === "unix") {
    const socketPath = value["socketPath"];
    if (typeof socketPath !== "string" || socketPath.length === 0) {
      throw new Error(`target ${host} unix socketPath must be a non-empty string`);
    }
    return { kind: "unix", socketPath };
  }

  if (value["kind"] === "tcp") {
    const targetHost = value["host"];
    const port = value["port"];
    if (typeof targetHost !== "string" || targetHost.length === 0) {
      throw new Error(`target ${host} tcp host must be a non-empty string`);
    }
    if (typeof port !== "number" || !Number.isInteger(port) || port < 0 || port > 65_535) {
      throw new Error(`target ${host} tcp port must be an integer from 0 to 65535`);
    }
    return { kind: "tcp", host: targetHost, port };
  }

  throw new Error(`target ${host} kind must be "unix" or "tcp"`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function messageFromUnknown(source: unknown): string {
  if (source instanceof Error) {
    return source.message;
  }
  return String(source);
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  startRelayFromEnv().catch((error: unknown) => {
    console.error(messageFromUnknown(error));
    process.exitCode = 1;
  });
}
