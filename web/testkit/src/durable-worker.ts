import { access } from "node:fs/promises";
import { dirname, join } from "node:path";

const WORKER_BINARY_NAME = "pohunek-sessiond";

export interface DurableWorkerFixtureOptions {
  /** Path to the `pohunekd` binary under test; the worker binary sits beside it. */
  readonly daemonBin: string;
}

export interface DurableWorkerFixture {
  /** Daemon spawn-env fragment selecting the subprocess worker launcher. */
  readonly env: Readonly<Record<string, string>>;
}

/**
 * Prepares a real daemon to run durable workers without a systemd user manager.
 *
 * Hosted CI (and containers generally) have no systemd user session or session
 * D-Bus, so the production systemd launcher cannot activate workers there. This
 * fixture points the daemon at the co-located `pohunek-sessiond` and selects the
 * subprocess launcher, which the daemon supervises directly; worker teardown
 * then rides on the daemon's own shutdown.
 */
export async function startDurableWorkerFixture(
  options: DurableWorkerFixtureOptions,
): Promise<DurableWorkerFixture> {
  const workerBin = process.env["POHUNEK_WORKER_BIN"]
    ?? join(dirname(options.daemonBin), WORKER_BINARY_NAME);
  await access(workerBin);

  return {
    env: {
      POHUNEK_WORKER_LAUNCHER: "subprocess",
      POHUNEK_WORKER_BIN: workerBin,
    },
  };
}
