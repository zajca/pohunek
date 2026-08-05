import { describe, expect, test } from "bun:test";
import {
  canRetryTerminal,
  terminalDimensionsChanged,
} from "../src/lib/terminal-connection";

describe("terminal retry state", () => {
  test("waits for the previous connect task to finalize", () => {
    expect(canRetryTerminal({ failed: true, connecting: true, closing: false })).toBeFalse();
    expect(canRetryTerminal({ failed: true, connecting: false, closing: false })).toBeTrue();
  });

  test("does not retry a healthy or closing terminal", () => {
    expect(canRetryTerminal({ failed: false, connecting: false, closing: false })).toBeFalse();
    expect(canRetryTerminal({ failed: true, connecting: false, closing: true })).toBeFalse();
  });
});

describe("terminal attach geometry", () => {
  test("detects a resize during attach negotiation", () => {
    expect(
      terminalDimensionsChanged(
        { cols: 120, rows: 40 },
        { cols: 100, rows: 30 },
      ),
    ).toBeTrue();
  });

  test("does not schedule a redundant resize for unchanged geometry", () => {
    expect(
      terminalDimensionsChanged(
        { cols: 120, rows: 40 },
        { cols: 120, rows: 40 },
      ),
    ).toBeFalse();
  });
});
