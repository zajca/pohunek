export type AppShortcut =
  | "command-palette"
  | "toggle-sidebar"
  | "new-session"
  | "inbox"
  | "next-blocked"
  | "focus-search"
  | "next-item"
  | "previous-item"
  | "activate-item"
  | "dismiss";

export interface KeybindingInput {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
  readonly shiftKey?: boolean;
  readonly repeat?: boolean;
  readonly isComposing?: boolean;
}

export type ShortcutTargetKind = "plain" | "editable" | "activation-control";

export type ShortcutHandler = (shortcut: AppShortcut, event: KeyboardEvent) => void;

/** Resolves a keyboard event without depending on browser globals, so mappings stay unit-testable. */
export function resolveKeybinding(input: KeybindingInput): AppShortcut | undefined {
  if (input.isComposing === true || input.repeat === true || input.altKey === true) {
    return undefined;
  }

  const key = input.key.toLowerCase();
  const commandModifier = input.ctrlKey === true || input.metaKey === true;
  if (commandModifier) {
    if (input.shiftKey === true) {
      return undefined;
    }
    if (key === "k") {
      return "command-palette";
    }
    return key === "b" ? "toggle-sidebar" : undefined;
  }
  if (input.shiftKey === true) {
    return undefined;
  }

  switch (key) {
    case "n":
      return "new-session";
    case "i":
      return "inbox";
    case "b":
      return "next-blocked";
    case "/":
      return "focus-search";
    case "arrowdown":
    case "j":
      return "next-item";
    case "arrowup":
    case "k":
      return "previous-item";
    case "enter":
      return "activate-item";
    case "escape":
      return "dismiss";
    default:
      return undefined;
  }
}

/** Returns true when global shortcuts would steal input from an editor, form control, or terminal. */
export function isEditableShortcutTarget(target: EventTarget | null): boolean {
  if (typeof Element === "undefined" || !(target instanceof Element)) {
    return false;
  }
  return target.closest("input, textarea, select, [contenteditable='true'], [role='textbox'], .xterm") !== null;
}

/** Decides whether a resolved shortcut may override the target's native keyboard behavior. */
export function shouldClaimKeybinding(shortcut: AppShortcut, targetKind: ShortcutTargetKind): boolean {
  if (targetKind === "editable") {
    return false;
  }
  return shortcut !== "activate-item" || targetKind !== "activation-control";
}

/** Installs application shortcuts and returns a cleanup function. */
export function installGlobalKeybindings(handler: ShortcutHandler): () => void {
  if (typeof window === "undefined") {
    return (): void => {};
  }

  const onKeydown = (event: KeyboardEvent): void => {
    if (event.defaultPrevented) {
      return;
    }
    const shortcut = resolveKeybinding(event);
    if (shortcut === undefined || !shouldClaimKeybinding(shortcut, shortcutTargetKind(event.target))) {
      return;
    }
    event.preventDefault();
    handler(shortcut, event);
  };

  window.addEventListener("keydown", onKeydown);
  return (): void => window.removeEventListener("keydown", onKeydown);
}

function shortcutTargetKind(target: EventTarget | null): ShortcutTargetKind {
  if (isEditableShortcutTarget(target)) {
    return "editable";
  }
  if (
    typeof Element !== "undefined"
    && target instanceof Element
    && target.closest("button, a[href], summary, [role='button'], [role='link']") !== null
  ) {
    return "activation-control";
  }
  return "plain";
}
