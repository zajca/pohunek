export interface TerminalRetryState {
  readonly failed: boolean;
  readonly connecting: boolean;
  readonly closing: boolean;
}

export interface TerminalDimensions {
  readonly cols: number;
  readonly rows: number;
}

/** Retry is safe only after the previous attach/read task has completely finalized. */
export function canRetryTerminal(state: TerminalRetryState): boolean {
  return state.failed && !state.connecting && !state.closing;
}

/** Reports whether terminal geometry changed while attach negotiation was in progress. */
export function terminalDimensionsChanged(
  initial: TerminalDimensions,
  current: TerminalDimensions,
): boolean {
  return initial.cols !== current.cols || initial.rows !== current.rows;
}
