export type BackendLogLevel = "info" | "warn" | "error";

export interface BackendLogEvent {
  readonly level: BackendLogLevel;
  readonly event: string;
  readonly method?: string;
  readonly host?: string;
  readonly duration_ms?: number;
  readonly status?: number | string;
  readonly lifecycle?: string;
  readonly error_class?: string;
}

export interface BackendLogger {
  log(event: BackendLogEvent): void;
}

export const stdoutLogger: BackendLogger = {
  log(event: BackendLogEvent): void {
    console.log(
      JSON.stringify({
        timestamp: new Date().toISOString(),
        component: "pohunek-backend",
        ...event,
      }),
    );
  },
};

export function errorClass(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
