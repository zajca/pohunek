import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access } from "node:fs/promises";
import { dirname, join } from "node:path";

const WORKER_READY_TIMEOUT_MS = 10_000;
const WORKER_READY_POLL_MS = 50;
const SYSTEMD_RUN = "systemd-run";
const SYSTEMCTL = "systemctl";

export interface DurableWorkerFixtureOptions {
  readonly daemonBin: string;
  readonly runtimeDir: string;
  readonly dataDir: string;
  readonly stateDir: string;
  readonly configDir: string;
  readonly sessionId: string;
}

export interface DurableWorkerFixture {
  readonly unitTemplate: string;
  stop(): Promise<void>;
}

/**
 * Starts one uniquely namespaced transient worker for real-daemon tests.
 *
 * The production daemon still addresses the worker through native systemd
 * StartUnit/RestartUnit calls. A unique template ensures a test can never
 * collide with or stop an operator's installed pohunek session unit.
 */
export async function startDurableWorkerFixture(
  options: DurableWorkerFixtureOptions,
): Promise<DurableWorkerFixture> {
  const workerBin = process.env["POHUNEK_WORKER_BIN"] ?? join(dirname(options.daemonBin), "pohunek-sessiond");
  await access(workerBin);

  const namespace = `pohunek-e2e-${process.pid}-${randomUUID().replaceAll("-", "")}`;
  const unitTemplate = `${namespace}@.service`;
  const unit = `${namespace}@${options.sessionId}.service`;
  const socket = join(options.runtimeDir, "pohunek", "workers", options.sessionId, "control.sock");
  await runCommand(SYSTEMD_RUN, [
    "--user",
    "--no-block",
    `--unit=${unit}`,
    "--property=Type=notify",
    "--property=NotifyAccess=main",
    "--property=Restart=no",
    "--property=KillMode=control-group",
    "--property=SendSIGHUP=yes",
    "--property=TimeoutStartSec=45s",
    "--property=TimeoutStopSec=30s",
    `--setenv=XDG_RUNTIME_DIR=${options.runtimeDir}`,
    `--setenv=XDG_DATA_HOME=${options.dataDir}`,
    `--setenv=XDG_STATE_HOME=${options.stateDir}`,
    `--setenv=XDG_CONFIG_HOME=${options.configDir}`,
    workerBin,
    "--session-id",
    options.sessionId,
  ]);

  try {
    await waitForPath(socket);
  } catch (error: unknown) {
    await stopUnit(unit);
    throw error;
  }

  return {
    unitTemplate,
    stop: async (): Promise<void> => {
      await stopUnit(unit);
    },
  };
}

async function waitForPath(path: string): Promise<void> {
  const deadline = Date.now() + WORKER_READY_TIMEOUT_MS;
  while (Date.now() < deadline) {
    try {
      await access(path);
      return;
    } catch {
      await new Promise((resolve) => {
        setTimeout(resolve, WORKER_READY_POLL_MS);
      });
    }
  }
  throw new Error(`durable worker did not create its socket: ${path}`);
}

async function stopUnit(unit: string): Promise<void> {
  await runCommand(SYSTEMCTL, ["--user", "stop", unit], true);
  await runCommand(SYSTEMCTL, ["--user", "reset-failed", unit], true);
}

async function runCommand(
  command: string,
  arguments_: readonly string[],
  tolerateFailure = false,
): Promise<void> {
  const child = spawn(command, arguments_, { stdio: ["ignore", "pipe", "pipe"] });
  const stdout: Buffer[] = [];
  const stderr: Buffer[] = [];
  child.stdout.on("data", (chunk: Buffer): void => {
    stdout.push(chunk);
  });
  child.stderr.on("data", (chunk: Buffer): void => {
    stderr.push(chunk);
  });
  const [code, signal] = await new Promise<[number | null, NodeJS.Signals | null]>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (exitCode, exitSignal) => {
      resolve([exitCode, exitSignal]);
    });
  });
  if (!tolerateFailure && (code !== 0 || signal !== null)) {
    throw new Error(
      `${command} failed (code=${String(code)}, signal=${String(signal)}): `
      + `${Buffer.concat(stdout).toString("utf8")}${Buffer.concat(stderr).toString("utf8")}`,
    );
  }
}
