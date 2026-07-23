import { describe, expect, test } from "bun:test";
import {
  formatStructuredError,
  structuredErrorDetails,
} from "../src/lib/errors";

describe("structured frontend errors", () => {
  test("formats client errors from public class and code fields", () => {
    const error = {
      errorClass: "daemon",
      code: "invalid_session",
      message: "human text that must not drive branching",
    };

    expect(structuredErrorDetails(error)).toEqual({
      errorClass: "daemon",
      code: "invalid_session",
    });
    expect(formatStructuredError(error)).toBe("daemon/invalid_session");
  });

  test("accepts a protocol error shape without inspecting msg", () => {
    expect(formatStructuredError({
      class: "runtime",
      code: "agent_unavailable",
      msg: "install an agent",
    })).toBe("runtime/agent_unavailable");
  });

  test("uses a stable fallback for unstructured failures", () => {
    expect(structuredErrorDetails(new Error("network details"))).toBeUndefined();
    expect(formatStructuredError(new Error("network details"))).toBe("unknown/unclassified_error");
    expect(formatStructuredError({ errorClass: "daemon", code: 42 })).toBe("unknown/unclassified_error");
  });
});
