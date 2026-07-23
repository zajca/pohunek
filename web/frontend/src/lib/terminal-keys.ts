export type TerminalToolbarKey =
  | "escape"
  | "tab"
  | "arrow-up"
  | "arrow-down"
  | "arrow-left"
  | "arrow-right"
  | "c";

export type TerminalModifier = "ctrl" | "alt";

export interface TerminalModifiers {
  readonly ctrl: boolean;
  readonly alt: boolean;
}

export const NO_TERMINAL_MODIFIERS: TerminalModifiers = Object.freeze({ ctrl: false, alt: false });

const ESCAPE = "\u001b";
const CONTROL_C = "\u0003";
const encoder = new TextEncoder();

/** Toggles one latched modifier without mutating the previous state. */
export function toggleTerminalModifier(
  modifiers: TerminalModifiers,
  modifier: TerminalModifier,
): TerminalModifiers {
  return modifier === "ctrl"
    ? { ...modifiers, ctrl: !modifiers.ctrl }
    : { ...modifiers, alt: !modifiers.alt };
}

/** Clears one-shot modifiers after a toolbar key has been sent. */
export function clearTerminalModifiers(): TerminalModifiers {
  return NO_TERMINAL_MODIFIERS;
}

/** Encodes a mobile-toolbar key as bytes accepted by a conventional xterm PTY. */
export function encodeTerminalToolbarKey(
  key: TerminalToolbarKey,
  modifiers: TerminalModifiers,
): Uint8Array {
  const arrowFinal = arrowFinalByte(key);
  if (arrowFinal !== undefined) {
    if (!modifiers.ctrl && !modifiers.alt) {
      return encoder.encode(`${ESCAPE}[${arrowFinal}`);
    }
    // Xterm modifier parameters are 1 + Alt(2) + Ctrl(4); Shift is not exposed here.
    const modifierParameter = 1 + (modifiers.alt ? 2 : 0) + (modifiers.ctrl ? 4 : 0);
    return encoder.encode(`${ESCAPE}[1;${modifierParameter}${arrowFinal}`);
  }

  let value: string;
  switch (key) {
    case "escape":
      value = ESCAPE;
      break;
    case "tab":
      // Ctrl+I and Tab share the same C0 control byte in a conventional terminal.
      value = "\t";
      break;
    case "c":
      value = modifiers.ctrl ? CONTROL_C : "c";
      break;
    case "arrow-up":
    case "arrow-down":
    case "arrow-left":
    case "arrow-right":
      throw new Error("arrow key did not resolve to a CSI final byte");
  }
  return encoder.encode(modifiers.alt ? `${ESCAPE}${value}` : value);
}

function arrowFinalByte(key: TerminalToolbarKey): string | undefined {
  switch (key) {
    case "arrow-up":
      return "A";
    case "arrow-down":
      return "B";
    case "arrow-right":
      return "C";
    case "arrow-left":
      return "D";
    case "escape":
    case "tab":
    case "c":
      return undefined;
  }
}
