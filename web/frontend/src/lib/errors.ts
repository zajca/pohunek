export interface StructuredErrorDetails {
  readonly errorClass: string;
  readonly code: string;
}

const UNKNOWN_ERROR_CLASS = "unknown";
const UNKNOWN_ERROR_CODE = "unclassified_error";

/** Extracts stable error fields without branching on a human-readable message. */
export function structuredErrorDetails(error: unknown): StructuredErrorDetails | undefined {
  if (!isRecord(error) || typeof error.code !== "string") {
    return undefined;
  }

  const errorClass = typeof error.errorClass === "string"
    ? error.errorClass
    : typeof error.class === "string" ? error.class : undefined;
  return errorClass === undefined ? undefined : { errorClass, code: error.code };
}

/** Formats only stable class/code fields and never inspects an error message. */
export function formatStructuredError(error: unknown): string {
  const details = structuredErrorDetails(error);
  return details === undefined
    ? `${UNKNOWN_ERROR_CLASS}/${UNKNOWN_ERROR_CODE}`
    : `${details.errorClass}/${details.code}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
