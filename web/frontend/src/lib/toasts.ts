import { writable, type Readable } from "svelte/store";
import { formatStructuredError } from "./errors";

export type ToastKind = "error" | "info" | "success";

export interface Toast {
  readonly id: number;
  readonly kind: ToastKind;
  readonly message: string;
}

const toastState = writable<readonly Toast[]>([]);
let nextToastId = 1;

export const toasts: Readable<readonly Toast[]> = toastState;

export function addToast(kind: ToastKind, message: string): number {
  const id = nextToastId;
  nextToastId += 1;
  toastState.update((current) => [...current, { id, kind, message }]);
  return id;
}

export function dismissToast(id: number): void {
  toastState.update((current) => current.filter((toast) => toast.id !== id));
}

export function addErrorToast(error: unknown): number {
  return addToast("error", formatStructuredError(error));
}
