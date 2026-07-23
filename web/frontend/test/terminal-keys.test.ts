import { describe, expect, test } from "bun:test";
import {
  clearTerminalModifiers,
  encodeTerminalToolbarKey,
  NO_TERMINAL_MODIFIERS,
  toggleTerminalModifier,
  type TerminalModifiers,
} from "../src/lib/terminal-keys";

const decoder = new TextDecoder();

describe("mobile terminal keys", () => {
  test("encodes unmodified terminal controls", () => {
    expect(decode("escape")).toBe("\u001b");
    expect(decode("tab")).toBe("\t");
    expect(decode("arrow-up")).toBe("\u001b[A");
    expect(decode("arrow-down")).toBe("\u001b[B");
    expect(decode("arrow-left")).toBe("\u001b[D");
    expect(decode("arrow-right")).toBe("\u001b[C");
  });

  test("encodes xterm arrow modifiers and Alt prefixes", () => {
    expect(decode("arrow-up", { ctrl: true, alt: false })).toBe("\u001b[1;5A");
    expect(decode("arrow-right", { ctrl: false, alt: true })).toBe("\u001b[1;3C");
    expect(decode("arrow-left", { ctrl: true, alt: true })).toBe("\u001b[1;7D");
    expect(decode("tab", { ctrl: false, alt: true })).toBe("\u001b\t");
  });

  test("encodes Control C as the interrupt byte", () => {
    expect(Array.from(encodeTerminalToolbarKey("c", { ctrl: true, alt: false }))).toEqual([0x03]);
    expect(Array.from(encodeTerminalToolbarKey("c", { ctrl: true, alt: true }))).toEqual([0x1b, 0x03]);
  });

  test("toggles immutable modifier state", () => {
    const ctrl = toggleTerminalModifier(NO_TERMINAL_MODIFIERS, "ctrl");
    const both = toggleTerminalModifier(ctrl, "alt");

    expect(NO_TERMINAL_MODIFIERS).toEqual({ ctrl: false, alt: false });
    expect(ctrl).toEqual({ ctrl: true, alt: false });
    expect(both).toEqual({ ctrl: true, alt: true });
    expect(toggleTerminalModifier(both, "ctrl")).toEqual({ ctrl: false, alt: true });
    expect(clearTerminalModifiers()).toEqual({ ctrl: false, alt: false });
  });
});

function decode(
  key: Parameters<typeof encodeTerminalToolbarKey>[0],
  modifiers: TerminalModifiers = NO_TERMINAL_MODIFIERS,
): string {
  return decoder.decode(encodeTerminalToolbarKey(key, modifiers));
}
