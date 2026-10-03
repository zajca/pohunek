import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { constants } from "node:fs";
import { access } from "node:fs/promises";
import { isAbsolute } from "node:path";

const DAEMON_READY_TIMEOUT_MS = 10_000;
const DAEMON_READY_POLL_MS = 50;
const DAEMON_TERM_GRACE_MS = 5_000;

/**
 * `git` shim for a daemon whose `PATH` holds only its fixture `bin` directory:
 * it forwards to the system git so project registration works inside the isolated environment.
 */
export const GIT_FIXTURE_SCRIPT = `#!/bin/sh
exec /usr/bin/git "$@"
`;

export interface ExitStatus {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
}

export interface DaemonProcessOptions {
  readonly daemonBin: string;
  readonly cwd: string;
  readonly env: NodeJS.ProcessEnv;
  /** Socket the daemon binds; readiness is the moment it appears. */
  readonly socketPath: string;
  /** Removes the run's directories; runs on readiness failure and on every `stop()`. */
  readonly removeRoots: () => Promise<void>;
}

export interface DaemonProcess {
  readonly socketPath: string;
  stdout(): string;
  stderr(): string;
  stop(): Promise<void>;
}

/** Diagnostic fields shared by every real-daemon harness. */
export interface DaemonContext {
  readonly tempRoot: string;
  readonly socketPath: string;
  stdout(): string;
  stderr(): string;
}

/**
 * Path of the `pohunekd` under test, taken from `POHUNEK_DAEMON_BIN`. The
 * variable is required and must be absolute: testkit also runs from an
 * installed release tarball, where no workspace build exists to fall back to.
 */
export function daemonBinaryPath(): string {
  const configured = process.env["POHUNEK_DAEMON_BIN"];
  if (configured === undefined || configured.length === 0) {
    throw new Error("POHUNEK_DAEMON_BIN must name the pohunekd binary under test");
  }
  if (!isAbsolute(configured)) {
    throw new Error("POHUNEK_DAEMON_BIN must be an absolute path");
  }
  return configured;
}

/**
 * Spawns `pohunekd` and waits for its socket. `stop()` terminates it, removes
 * the run's roots even when the daemon exits uncleanly, and reports every
 * teardown failure.
 */
export async function startDaemonProcess(options: DaemonProcessOptions): Promise<DaemonProcess> {
  const { daemonBin, socketPath, removeRoots } = options;
  const stdoutChunks: string[] = [];
  const stderrChunks: string[] = [];
  let exitStatus: ExitStatus | undefined;
  let spawnError: Error | undefined;

  const child = spawn(daemonBin, [], {
    cwd: options.cwd,
    env: options.env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk: string): void => {
    stdoutChunks.push(chunk);
  });
  child.stderr.on("data", (chunk: string): void => {
    stderrChunks.push(chunk);
  });
  child.once("error", (error: Error): void => {
    spawnError = error;
  });
  const exitPromise = once(child, "exit").then(([code, signal]) => {
    exitStatus = { code: code as number | null, signal: signal as NodeJS.Signals | null };
    return exitStatus;
  });

  const logs = (): Pick<DaemonProcess, "stdout" | "stderr"> => ({
    stdout: () => stdoutChunks.join(""),
    stderr: () => stderrChunks.join(""),
  });

  try {
    await waitForDaemonSocket(socketPath, () => exitStatus, () => spawnError, logs);
  } catch (error: unknown) {
    await stopChild(child, exitPromise, () => exitStatus).catch(() => undefined);
    await removeRoots();
    throw error;
  }

  return {
    socketPath,
    ...logs(),
    stop: async (): Promise<void> => {
      // Roots are removed whether or not the daemon stopped cleanly: a leaked
      // default runtime directory would block the next run and the user's daemon.
      const failures: unknown[] = [];
      let status: ExitStatus | undefined;
      try {
        status = await stopChild(child, exitPromise, () => exitStatus);
      } catch (error: unknown) {
        failures.push(error);
      }
      try {
        await removeRoots();
      } catch (error: unknown) {
        failures.push(error);
      }
      if (status !== undefined && (status.code !== 0 || status.signal !== null)) {
        failures.push(new Error(
          `pohunekd exited uncleanly (code=${String(status.code)}, signal=${String(status.signal)})\n`
            + `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
        ));
      }
      if (failures.length > 1) {
        throw new AggregateError(failures, "daemon teardown failed");
      }
      if (failures.length === 1) {
        throw errorFromUnknown(failures[0]);
      }
    },
  };
}

/**
 * Runs `run` against `resource`, then always tears the resource down. When both
 * fail the two errors are reported together under `teardownMessage`.
 */
export async function withResource<R, T>(
  resource: R,
  run: (resource: R) => Promise<T>,
  teardown: (resource: R) => Promise<void>,
  teardownMessage: string,
): Promise<T> {
  let result: T | undefined;
  let failure: unknown;

  try {
    result = await run(resource);
  } catch (error: unknown) {
    failure = error;
  }

  try {
    await teardown(resource);
  } catch (error: unknown) {
    if (failure !== undefined) {
      throw new AggregateError([failure, error], teardownMessage);
    }
    throw error;
  }

  if (failure !== undefined) {
    throw errorFromUnknown(failure);
  }
  return result as T;
}

/** Appends the daemon's roots, socket and captured output to a scenario failure. */
export function addDaemonContext(error: unknown, daemon: DaemonContext): Error {
  const message = error instanceof Error ? error.message : String(error);
  const wrapped = new Error(
    `${message}\n`
      + `daemon temp root: ${daemon.tempRoot}\n`
      + `daemon socket: ${daemon.socketPath}\n`
      + `daemon stdout:\n${daemon.stdout()}\n`
      + `daemon stderr:\n${daemon.stderr()}`,
  );
  if (error instanceof Error && error.stack !== undefined) {
    wrapped.stack = error.stack;
  }
  return wrapped;
}

export function withTimeout<T>(promise: Promise<T>, timeoutMs: number, message: string): Promise<T> {
  return new Promise((resolvePromise, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(message));
    }, timeoutMs);

    promise.then(
      (value) => {
        clearTimeout(timer);
        resolvePromise(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(errorFromUnknown(error));
      },
    );
  });
}

export function delay(ms: number): Promise<void> {
  return new Promise((resolvePromise) => {
    setTimeout(resolvePromise, ms);
  });
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export function errorFromUnknown(error: unknown): Error {
  if (error instanceof Error) {
    return error;
  }
  return new Error(String(error));
}

function formatUnknown(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

async function waitForDaemonSocket(
  socketPath: string,
  exitStatus: () => ExitStatus | undefined,
  spawnError: () => Error | undefined,
  logs: () => Pick<DaemonProcess, "stdout" | "stderr">,
): Promise<void> {
  const deadline = Date.now() + DAEMON_READY_TIMEOUT_MS;
  let lastError: unknown;

  while (Date.now() < deadline) {
    const currentSpawnError = spawnError();
    if (currentSpawnError !== undefined) {
      throw currentSpawnError;
    }
    const status = exitStatus();
    if (status !== undefined) {
      throw new Error(
        `pohunekd exited before readiness (code=${String(status.code)}, signal=${String(status.signal)})\n`
          + `socket: ${socketPath}\nstdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
      );
    }

    try {
      await access(socketPath, constants.F_OK);
      return;
    } catch (error: unknown) {
      lastError = error;
      await delay(DAEMON_READY_POLL_MS);
    }
  }

  throw new Error(
    `pohunekd did not expose its socket within ${DAEMON_READY_TIMEOUT_MS}ms\n`
      + `socket: ${socketPath}\nlast error: ${formatUnknown(lastError)}\n`
      + `stdout:\n${logs().stdout()}\nstderr:\n${logs().stderr()}`,
  );
}

async function stopChild(
  child: ChildProcess,
  exitPromise: Promise<ExitStatus>,
  exitStatus: () => ExitStatus | undefined,
): Promise<ExitStatus> {
  const current = exitStatus();
  if (current !== undefined) {
    return current;
  }

  child.kill("SIGTERM");
  try {
    return await withTimeout(
      exitPromise,
      DAEMON_TERM_GRACE_MS,
      `pohunekd did not exit within ${DAEMON_TERM_GRACE_MS}ms after SIGTERM`,
    );
  } catch (error: unknown) {
    child.kill("SIGKILL");
    await exitPromise;
    throw error;
  }
}
