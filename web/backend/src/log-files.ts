import {
  closeSync,
  constants,
  fstatSync,
  lstatSync,
  mkdirSync,
  openSync,
  readdirSync,
  renameSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { join } from "node:path";
import type { BackendLogEvent, BackendLogger } from "./log";

/** Active log file; rotated files carry a numeric suffix (`.1` newest). */
export const LOG_FILE_NAME = "pohunek-backend.jsonl";

/** Owner-only modes, like the Rust log family (`crates/logging`). */
const DIRECTORY_MODE = 0o700;
const FILE_MODE = 0o600;

export interface RotatingLogOptions {
  readonly dir: string;
  /** Rotates before an event would push the active file above this size. */
  readonly maxFileBytes: number;
  /** Files kept including the active one. */
  readonly maxFiles: number;
}

export class LogFileError extends Error {
  public override readonly name = "LogFileError";
}

/**
 * Appends one JSON object per line to a size-bounded, owner-private file family.
 *
 * Total disk use stays within `maxFileBytes * maxFiles`. A single event larger
 * than `maxFileBytes` is replaced by a fixed notice so a runaway event cannot
 * defeat the bound. Writes are synchronous: ordering matches the event order
 * and a crash loses nothing already logged.
 */
export function rotatingFileLogger(options: RotatingLogOptions): BackendLogger {
  const { dir, maxFileBytes, maxFiles } = options;
  if (!Number.isInteger(maxFileBytes) || maxFileBytes <= 0 || !Number.isInteger(maxFiles) || maxFiles <= 0) {
    throw new LogFileError("log limits must be positive integers");
  }
  prepareDirectory(dir);
  const active = join(dir, LOG_FILE_NAME);
  pruneBeyondLimit(dir, maxFiles);
  let descriptor = openActive(active);
  let size = fstatSync(descriptor).size;

  const rotate = (): void => {
    closeSync(descriptor);
    if (maxFiles > 1) {
      removeIfPresent(rotatedName(dir, maxFiles - 1));
      for (let index = maxFiles - 2; index >= 1; index -= 1) {
        renameIfPresent(rotatedName(dir, index), rotatedName(dir, index + 1));
      }
      renameSync(active, rotatedName(dir, 1));
    } else {
      removeIfPresent(active);
    }
    descriptor = openActive(active);
    size = 0;
  };

  return {
    log(event: BackendLogEvent): void {
      let line = encodeLine(event);
      if (line.byteLength > maxFileBytes) {
        line = encodeLine({
          level: "warn",
          event: "log_event_dropped",
          status: "oversize",
        });
        if (line.byteLength > maxFileBytes) {
          return;
        }
      }
      if (size > 0 && size + line.byteLength > maxFileBytes) {
        rotate();
      }
      writeAll(descriptor, line);
      size += line.byteLength;
    },
  };
}

function encodeLine(event: BackendLogEvent): Buffer {
  return Buffer.from(
    `${JSON.stringify({
      timestamp: new Date().toISOString(),
      component: "pohunek-backend",
      ...event,
    })}\n`,
  );
}

function writeAll(descriptor: number, bytes: Buffer): void {
  let offset = 0;
  while (offset < bytes.byteLength) {
    offset += writeSync(descriptor, bytes, offset);
  }
}

function rotatedName(dir: string, index: number): string {
  return join(dir, `${LOG_FILE_NAME}.${index}`);
}

function prepareDirectory(dir: string): void {
  mkdirSync(dir, { recursive: true, mode: DIRECTORY_MODE });
  const info = lstatSync(dir);
  if (info.isSymbolicLink() || !info.isDirectory()) {
    throw new LogFileError(`log directory is not a real directory: ${dir}`);
  }
  if (typeof process.geteuid === "function" && info.uid !== process.geteuid()) {
    throw new LogFileError(`log directory is not owned by the current user: ${dir}`);
  }
  if ((info.mode & 0o077) !== 0) {
    throw new LogFileError(`log directory must not be accessible to group or others: ${dir}`);
  }
}

function openActive(path: string): number {
  // O_NOFOLLOW makes a symlinked active file an error instead of a write elsewhere.
  const descriptor = openSync(
    path,
    constants.O_WRONLY | constants.O_APPEND | constants.O_CREAT | constants.O_NOFOLLOW,
    FILE_MODE,
  );
  if (!fstatSync(descriptor).isFile()) {
    closeSync(descriptor);
    throw new LogFileError(`log file is not a regular file: ${path}`);
  }
  return descriptor;
}

/** Removes rotated files from an earlier, larger `maxFiles`, so the bound holds. */
function pruneBeyondLimit(dir: string, maxFiles: number): void {
  const prefix = `${LOG_FILE_NAME}.`;
  for (const name of readdirSync(dir)) {
    if (!name.startsWith(prefix)) {
      continue;
    }
    const suffix = name.slice(prefix.length);
    if (/^[1-9][0-9]*$/.test(suffix) && Number(suffix) >= maxFiles) {
      removeIfPresent(join(dir, name));
    }
  }
}

function removeIfPresent(path: string): void {
  try {
    unlinkSync(path);
  } catch (error: unknown) {
    if (!isMissing(error)) {
      throw error;
    }
  }
}

function renameIfPresent(from: string, to: string): void {
  try {
    renameSync(from, to);
  } catch (error: unknown) {
    if (!isMissing(error)) {
      throw error;
    }
  }
}

function isMissing(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}
