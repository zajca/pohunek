import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { MIN_PROTOCOL_VERSION } from "@pohunek/protocol";

/**
 * Payloads recorded from the previous release by its own code. They are the
 * same files the Rust adapter and client tests use, so neither side of an
 * exchange with the previous daemon is written by hand.
 */
const fixturesDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../crates/protocol/tests/fixtures/compat/v3",
);

export interface RecordedRequest { method: string; params: unknown }
export interface RecordedResult { method: string; result: unknown }
export interface RecordedEvent { event: string; payload: Record<string, unknown> }

function entries<T>(file: string): T[] {
  const document = JSON.parse(readFileSync(join(fixturesDir, file), "utf8")) as {
    release: string;
    protocol_version: number;
    entries: T[];
  };
  if (document.release !== "v0.33.0" || document.protocol_version !== MIN_PROTOCOL_VERSION) {
    throw new Error(`${file} does not record the previous release`);
  }
  return document.entries;
}

export const recordedRequests = (): RecordedRequest[] => entries<RecordedRequest>("requests.json");
export const recordedResults = (): RecordedResult[] => entries<RecordedResult>("results.json");
export const recordedEvents = (): RecordedEvent[] => entries<RecordedEvent>("events.json");

/** Keys the previous release spelled differently, as `[previous, current]`. Independent of the SDK adapter. */
const BLIND_RENAMES: ReadonlyArray<readonly [string, string]> = [
  ["runtime_id", "worker_instance_id"],
  ["previous_runtime_id", "previous_worker_instance_id"],
];

/** Renames previous-release object keys to the current spelling, everywhere. */
export function renameKeysEverywhere(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(renameKeysEverywhere);
  }
  if (typeof value === "object" && value !== null) {
    return Object.fromEntries(
      Object.entries(value).map(([key, inner]) => [
        BLIND_RENAMES.find(([previous]) => previous === key)?.[1] ?? key,
        renameKeysEverywhere(inner),
      ]),
    );
  }
  return value;
}

export function containsKey(value: unknown, wanted: string): boolean {
  if (Array.isArray(value)) {
    return value.some((inner) => containsKey(inner, wanted));
  }
  if (typeof value === "object" && value !== null) {
    return Object.entries(value).some(([key, inner]) => key === wanted || containsKey(inner, wanted));
  }
  return false;
}

export const previousKeys = (): string[] => BLIND_RENAMES.map(([previous]) => previous);
