export interface SelectedSessionState {
  readonly host: string;
  readonly sessionId: string;
}

export interface PersistedUiState {
  readonly selectedSession?: SelectedSessionState;
  readonly sidebarCollapsed: boolean;
}

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export const UI_STATE_STORAGE_KEY = "pohunek.control-center.ui.v1";

const DEFAULT_UI_STATE: PersistedUiState = Object.freeze({ sidebarCollapsed: false });

/** Loads UI preferences without allowing malformed or unavailable storage to break startup. */
export function loadUiState(storage: StorageLike | undefined = browserStorage()): PersistedUiState {
  if (storage === undefined) {
    return DEFAULT_UI_STATE;
  }

  try {
    const raw = storage.getItem(UI_STATE_STORAGE_KEY);
    if (raw === null) {
      return DEFAULT_UI_STATE;
    }
    return parseUiState(JSON.parse(raw));
  } catch {
    return DEFAULT_UI_STATE;
  }
}

/** Saves UI preferences while tolerating privacy mode and storage quota failures. */
export function saveUiState(
  state: PersistedUiState,
  storage: StorageLike | undefined = browserStorage(),
): void {
  if (storage === undefined) {
    return;
  }
  try {
    storage.setItem(UI_STATE_STORAGE_KEY, JSON.stringify(parseUiState(state)));
  } catch {
    // Persistence is optional; the in-memory UI remains usable when storage is unavailable.
  }
}

export function parseUiState(value: unknown): PersistedUiState {
  if (!isRecord(value)) {
    return DEFAULT_UI_STATE;
  }

  const sidebarCollapsed = value["sidebarCollapsed"] === true;
  const selected = value["selectedSession"];
  if (!isRecord(selected)) {
    return { sidebarCollapsed };
  }

  const host = selected["host"];
  const sessionId = selected["sessionId"];
  if (typeof host !== "string" || host.length === 0 || typeof sessionId !== "string" || sessionId.length === 0) {
    return { sidebarCollapsed };
  }
  return { sidebarCollapsed, selectedSession: { host, sessionId } };
}

function browserStorage(): StorageLike | undefined {
  if (typeof window === "undefined") {
    return undefined;
  }
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
