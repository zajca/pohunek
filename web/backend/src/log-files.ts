import {
  closeSync,
  constants,
  fchmodSync,
  fstatSync,
  ftruncateSync,
  lstatSync,
  mkdirSync,
  openSync,
  readdirSync,
  renameSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { join } from "node:path";
import { errorClass, stdoutLogger, type BackendLogEvent, type BackendLogger } from "./log";

/** Active log file; rotated files carry a numeric suffix (`.1` newest). */
export const LOG_FILE_NAME = "pohunek-backend.jsonl";

/** Owner-only modes, like the Rust log family (`crates/logging`). */
const DIRECTORY_MODE = 0o700;
const FILE_MODE = 0o600;
const GROUP_AND_OTHER_BITS = 0o077;

const NO_DESCRIPTOR = -1;

export interface RotatingLogOptions {
  readonly dir: string;
  /** Rotates before an event would push the active file above this size. */
  readonly maxFileBytes: number;
  /** Files kept including the active one. */
  readonly maxFiles: number;
  /** Receives events the files cannot take; defaults to standard output. */
  readonly fallback?: BackendLogger;
}

export interface ClosableBackendLogger extends BackendLogger {
  /** Releases the active file; later events go to the fallback. Idempotent. */
  close(): void;
}

export class LogFileError extends Error {
  public override readonly name = "LogFileError";
}

/**
 * Appends one JSON object per line to a size-bounded, owner-private file family.
 *
 * Total disk use stays within `maxFileBytes * maxFiles`: files left by an
 * earlier run above the bound are removed (the active one is emptied) when the
 * logger opens. A single event larger than `maxFileBytes` is replaced by a
 * fixed notice. Writes are synchronous so ordering matches the event order.
 *
 * A failing write or rotation (full disk, I/O error) never reaches the caller:
 * the event goes to the fallback logger, one `log_file_failed` event reports the
 * failure, and the next event tries the files again.
 */
export function rotatingFileLogger(options: RotatingLogOptions): ClosableBackendLogger {
  const { dir, maxFileBytes, maxFiles } = options;
  const fallback = options.fallback ?? stdoutLogger;
  if (!Number.isInteger(maxFileBytes) || maxFileBytes <= 0 || !Number.isInteger(maxFiles) || maxFiles <= 0) {
    throw new LogFileError("log limits must be positive integers");
  }
  prepareDirectory(dir);
  const active = join(dir, LOG_FILE_NAME);
  sanitizeRotated(dir, maxFiles, maxFileBytes);
  let descriptor = openActive(active, maxFileBytes);
  let size = fstatSync(descriptor).size;
  let closed = false;
  let failing = false;

  const ensureOpen = (): void => {
    if (descriptor === NO_DESCRIPTOR) {
      descriptor = openActive(active, maxFileBytes);
      size = fstatSync(descriptor).size;
    }
  };

  const rotate = (): void => {
    closeSync(descriptor);
    descriptor = NO_DESCRIPTOR;
    if (maxFiles > 1) {
      removeIfPresent(rotatedName(dir, maxFiles - 1));
      for (let index = maxFiles - 2; index >= 1; index -= 1) {
        renameIfPresent(rotatedName(dir, index), rotatedName(dir, index + 1));
      }
      renameSync(active, rotatedName(dir, 1));
    } else {
      removeIfPresent(active);
    }
    descriptor = openActive(active, maxFileBytes);
    size = 0;
  };

  const append = (event: BackendLogEvent): void => {
    let line = encodeLine(event);
    if (line.byteLength > maxFileBytes) {
      line = encodeLine({ level: "warn", event: "log_event_dropped", status: "oversize" });
      if (line.byteLength > maxFileBytes) {
        return;
      }
    }
    ensureOpen();
    if (size > 0 && size + line.byteLength > maxFileBytes) {
      rotate();
    }
    writeAll(descriptor, line);
    size += line.byteLength;
  };

  return {
    log(event: BackendLogEvent): void {
      if (closed) {
        fallback.log(event);
        return;
      }
      try {
        append(event);
        failing = false;
      } catch (error: unknown) {
        if (!failing) {
          failing = true;
          fallback.log({
            level: "error",
            event: "log_file_failed",
            status: "failed",
            error_class: errorClass(error),
          });
        }
        fallback.log(event);
      }
    },
    close(): void {
      if (closed) {
        return;
      }
      closed = true;
      if (descriptor !== NO_DESCRIPTOR) {
        closeSync(descriptor);
        descriptor = NO_DESCRIPTOR;
      }
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
  requireCurrentOwner(info.uid, dir);
  if ((info.mode & GROUP_AND_OTHER_BITS) !== 0) {
    throw new LogFileError(`log directory must not be accessible to group or others: ${dir}`);
  }
}

function requireCurrentOwner(uid: number, path: string): void {
  if (typeof process.geteuid === "function" && uid !== process.geteuid()) {
    throw new LogFileError(`log path is not owned by the current user: ${path}`);
  }
}

/**
 * Opens the active file without following a symlink, requires a regular file
 * owned by the current user, forces mode 0600 through the descriptor, and
 * empties a file left above the size bound. `O_NONBLOCK` keeps a FIFO in the
 * slot from blocking startup (it fails with `ENXIO`); regular files ignore it.
 */
function openActive(path: string, maxFileBytes: number): number {
  const descriptor = openSync(
    path,
    constants.O_WRONLY | constants.O_APPEND | constants.O_CREAT | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    FILE_MODE,
  );
  try {
    const info = fstatSync(descriptor);
    if (!info.isFile()) {
      throw new LogFileError(`log file is not a regular file: ${path}`);
    }
    requireCurrentOwner(info.uid, path);
    if ((info.mode & GROUP_AND_OTHER_BITS) !== 0) {
      fchmodSync(descriptor, FILE_MODE);
    }
    if (info.size > maxFileBytes) {
      ftruncateSync(descriptor, 0);
    }
  } catch (error: unknown) {
    closeSync(descriptor);
    throw error;
  }
  return descriptor;
}

/**
 * Brings rotated files left by an earlier run inside the bound: files from a
 * larger `maxFiles` and files above the size limit are removed, the rest are
 * checked for ownership and forced to mode 0600.
 */
function sanitizeRotated(dir: string, maxFiles: number, maxFileBytes: number): void {
  const prefix = `${LOG_FILE_NAME}.`;
  for (const name of readdirSync(dir)) {
    const suffix = name.startsWith(prefix) ? name.slice(prefix.length) : "";
    if (!/^[1-9][0-9]*$/.test(suffix)) {
      continue;
    }
    const path = join(dir, name);
    if (Number(suffix) >= maxFiles) {
      removeIfPresent(path);
      continue;
    }
    // O_NOFOLLOW: a symlink in a rotated slot is removed, never followed.
    // O_NONBLOCK: a FIFO opens without waiting for a writer and is then refused.
    let descriptor: number;
    try {
      descriptor = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    } catch (error: unknown) {
      if (hasCode(error, "ELOOP")) {
        removeIfPresent(path);
        continue;
      }
      if (isMissing(error)) {
        continue;
      }
      throw error;
    }
    try {
      const info = fstatSync(descriptor);
      if (!info.isFile()) {
        throw new LogFileError(`rotated log file is not a regular file: ${path}`);
      }
      requireCurrentOwner(info.uid, path);
      if (info.size > maxFileBytes) {
        removeIfPresent(path);
      } else if ((info.mode & GROUP_AND_OTHER_BITS) !== 0) {
        fchmodSync(descriptor, FILE_MODE);
      }
    } finally {
      closeSync(descriptor);
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
  return hasCode(error, "ENOENT");
}

function hasCode(error: unknown, code: string): boolean {
  return error instanceof Error && "code" in error && error.code === code;
}
