import { describe, expect, test } from "bun:test";
import {
  CompatError,
  METHOD_NAMES,
  MIN_PROTOCOL_VERSION,
  PREVIOUS_VERSION_INTRODUCED_METHODS,
  PROTOCOL_VERSION,
  downgradeRequestParams,
  upgradeEventPayload,
  upgradeResult,
} from "@pohunek/sdk";
import {
  containsKey,
  previousKeys,
  recordedEvents,
  recordedRequests,
  recordedResults,
  renameKeysEverywhere,
} from "./previous-release-fixtures";

const PREVIOUS = MIN_PROTOCOL_VERSION;

describe("previous-version adapter", () => {
  test("the window reaches the release the fixtures were recorded from", () => {
    expect(PREVIOUS).toBe(3);
    expect(PROTOCOL_VERSION).toBe(PREVIOUS + 1);
  });

  test("every registered method is recorded or new in the current protocol", () => {
    const recorded = new Set(recordedRequests().map((entry) => entry.method));
    const introduced = new Set<string>(PREVIOUS_VERSION_INTRODUCED_METHODS);
    for (const name of METHOD_NAMES) {
      expect(recorded.has(name) !== introduced.has(name)).toBe(true);
    }
  });

  test("current requests downgrade to the recorded previous-release request", () => {
    let renamed = 0;
    for (const { method, params } of recordedRequests()) {
      const current = renameKeysEverywhere(params);
      const downgraded = downgradeRequestParams(PREVIOUS, method, current);
      expect(downgraded).toEqual(params);
      for (const key of previousKeys()) {
        expect(containsKey(current, key)).toBe(false);
      }
      if (JSON.stringify(current) !== JSON.stringify(params)) {
        renamed += 1;
      }
    }
    expect(renamed).toBeGreaterThanOrEqual(3);
  });

  test("recorded previous-release results upgrade to the current shape", () => {
    let renamed = 0;
    for (const { method, result } of recordedResults()) {
      const upgraded = upgradeResult(PREVIOUS, method, result);
      expect(upgraded).toEqual(renameKeysEverywhere(result));
      if (JSON.stringify(upgraded) !== JSON.stringify(result)) {
        renamed += 1;
      }
    }
    expect(renamed).toBeGreaterThanOrEqual(8);
  });

  test("recorded previous-release events upgrade to the current shape", () => {
    let renamed = 0;
    for (const { event, payload } of recordedEvents()) {
      const upgraded = upgradeEventPayload(PREVIOUS, event, payload);
      expect(upgraded).toEqual(renameKeysEverywhere(payload));
      if (JSON.stringify(upgraded) !== JSON.stringify(payload)) {
        renamed += 1;
      }
    }
    expect(renamed).toBeGreaterThanOrEqual(9);
  });

  test("the adapter never mutates its input", () => {
    const recorded = recordedRequests().find((entry) => entry.method === "session.report_native_id");
    const current = renameKeysEverywhere(recorded?.params);
    const before = JSON.stringify(current);
    downgradeRequestParams(PREVIOUS, "session.report_native_id", current);
    expect(JSON.stringify(current)).toBe(before);
  });

  test("the current version passes through untouched", () => {
    const params = { runtime: { worker_instance_id: "w-1", runtime_generation: "1" } };
    expect(downgradeRequestParams(PROTOCOL_VERSION, "session.output", params)).toBe(params);
    expect(upgradeResult(PROTOCOL_VERSION, "session.output", params)).toBe(params);
  });

  test("a version outside the window has no adapter", () => {
    expect(() => downgradeRequestParams(PREVIOUS - 1, "session.list", {})).toThrow(CompatError);
    expect(() => upgradeResult(PREVIOUS - 1, "session.list", [])).toThrow(CompatError);
  });

  test("methods the previous release never defined are refused in both directions", () => {
    for (const name of PREVIOUS_VERSION_INTRODUCED_METHODS) {
      expect(() => downgradeRequestParams(PREVIOUS, name, null)).toThrow(CompatError);
      expect(() => upgradeResult(PREVIOUS, name, {})).toThrow(CompatError);
    }
  });

  test("a result that already has the current spelling cannot be translated", () => {
    expect(() => upgradeResult(PREVIOUS, "session.read", { runtime_id: "w-1", worker_instance_id: "w-2" }))
      .toThrow(CompatError);
    expect(() => downgradeRequestParams(PREVIOUS, "session.report_native_id", {
      worker_instance_id: "w-1",
      runtime_id: "w-0",
    })).toThrow(CompatError);
  });

  test("free-form values are never walked", () => {
    const params = { session_id: "s-1", metadata: { worker_instance_id: "user-value" } };
    expect(downgradeRequestParams(PREVIOUS, "session.set_metadata", params)).toEqual(params);
    const result = { session: { metadata: { runtime_id: "user-value" } } };
    expect(upgradeResult(PREVIOUS, "session.new", result)).toEqual(result);
  });
});
