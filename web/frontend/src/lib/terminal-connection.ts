export interface TerminalRetryState {
  readonly failed: boolean;
  readonly connecting: boolean;
  readonly closing: boolean;
}

/** Retry is safe only after the previous attach/read task has completely finalized. */
export function canRetryTerminal(state: TerminalRetryState): boolean {
  return state.failed && !state.connecting && !state.closing;
}
