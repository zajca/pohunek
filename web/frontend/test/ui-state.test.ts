import { describe, expect, test } from "bun:test";
import {
  loadUiState,
  parseUiState,
  saveUiState,
  UI_STATE_STORAGE_KEY,
  type StorageLike,
} from "../src/lib/ui-state";

class MemoryStorage implements StorageLike {
  public value: string | null = null;

  public getItem(key: string): string | null {
    expect(key).toBe(UI_STATE_STORAGE_KEY);
    return this.value;
  }

  public setItem(key: string, value: string): void {
    expect(key).toBe(UI_STATE_STORAGE_KEY);
    this.value = value;
  }
}

describe("persisted UI state", () => {
  test("round-trips the last selected session and sidebar state", () => {
    const storage = new MemoryStorage();
    const state = {
      sidebarCollapsed: true,
      selectedSession: { host: "dev peer", sessionId: "s-1" },
    } as const;

    saveUiState(state, storage);

    expect(loadUiState(storage)).toEqual(state);
  });

  test("uses safe defaults for malformed or incomplete data", () => {
    expect(parseUiState(null)).toEqual({ sidebarCollapsed: false });
    expect(parseUiState({ sidebarCollapsed: "yes" })).toEqual({ sidebarCollapsed: false });
    expect(parseUiState({ sidebarCollapsed: true, selectedSession: { host: "", sessionId: "s-1" } }))
      .toEqual({ sidebarCollapsed: true });

    const storage = new MemoryStorage();
    storage.value = "{not-json";
    expect(loadUiState(storage)).toEqual({ sidebarCollapsed: false });
  });

  test("tolerates unavailable storage", () => {
    const failingStorage: StorageLike = {
      getItem(): string | null {
        throw new Error("unavailable");
      },
      setItem(): void {
        throw new Error("unavailable");
      },
    };

    expect(loadUiState(failingStorage)).toEqual({ sidebarCollapsed: false });
    expect(() => saveUiState({ sidebarCollapsed: false }, failingStorage)).not.toThrow();
  });
});
