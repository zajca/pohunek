import { describe, expect, test } from "bun:test";
import { resolveKeybinding, shouldClaimKeybinding } from "../src/lib/keybindings";

describe("global keybindings", () => {
  test("maps navigation and overlay shortcuts", () => {
    expect(resolveKeybinding({ key: "k", ctrlKey: true })).toBe("command-palette");
    expect(resolveKeybinding({ key: "k", metaKey: true })).toBe("command-palette");
    expect(resolveKeybinding({ key: "b", ctrlKey: true })).toBe("toggle-sidebar");
    expect(resolveKeybinding({ key: "b", metaKey: true })).toBe("toggle-sidebar");
    expect(resolveKeybinding({ key: "N" })).toBe("new-session");
    expect(resolveKeybinding({ key: "i" })).toBe("inbox");
    expect(resolveKeybinding({ key: "b" })).toBe("next-blocked");
    expect(resolveKeybinding({ key: "/" })).toBe("focus-search");
    expect(resolveKeybinding({ key: "ArrowDown" })).toBe("next-item");
    expect(resolveKeybinding({ key: "j" })).toBe("next-item");
    expect(resolveKeybinding({ key: "ArrowUp" })).toBe("previous-item");
    expect(resolveKeybinding({ key: "Enter" })).toBe("activate-item");
    expect(resolveKeybinding({ key: "Escape" })).toBe("dismiss");
  });

  test("does not claim modified, repeated, or composing keystrokes", () => {
    expect(resolveKeybinding({ key: "n", ctrlKey: true })).toBeUndefined();
    expect(resolveKeybinding({ key: "n", altKey: true })).toBeUndefined();
    expect(resolveKeybinding({ key: "n", shiftKey: true })).toBeUndefined();
    expect(resolveKeybinding({ key: "n", repeat: true })).toBeUndefined();
    expect(resolveKeybinding({ key: "n", isComposing: true })).toBeUndefined();
    expect(resolveKeybinding({ key: "x" })).toBeUndefined();
  });

  test("preserves native Enter activation on controls", () => {
    expect(shouldClaimKeybinding("activate-item", "activation-control")).toBeFalse();
    expect(shouldClaimKeybinding("activate-item", "plain")).toBeTrue();
    expect(shouldClaimKeybinding("next-item", "activation-control")).toBeTrue();
    expect(shouldClaimKeybinding("command-palette", "activation-control")).toBeTrue();
    expect(shouldClaimKeybinding("command-palette", "editable")).toBeFalse();
  });
});
