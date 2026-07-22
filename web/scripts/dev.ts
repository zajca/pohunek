import { appendFile, mkdir } from "node:fs/promises";
import { spawn, type ChildProcess } from "node:child_process";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import type { BackendLogEvent, BackendLogger } from "@pohunek/backend";
import {
  FIXTURE_LOCAL_HOST,
  FIXTURE_PEER_HOST,
  startFixtureStack,
  type FixtureStackHandle,
} from "./fixture-stack";

const WEB_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const LOGS_DIR = join(WEB_ROOT, "logs");
const DEV_LOG_PATH = join(LOGS_DIR, "dev.log");
const CLEAN_EXIT_CODE = 0;
const FAILURE_EXIT_CODE = 1;
const VITE_BACKEND_URL_ENV = "POHUNEK_VITE_BACKEND_URL";
const NODE_EXECUTABLE_ENV = "POHUNEK_NODE_BIN";
const DEFAULT_NODE_EXECUTABLE = "node";
const VITE_READY_PREFIX = "POHUNEK_VITE_READY ";
const VITE_CHILD_PATH = join(WEB_ROOT, "scripts", "vite-dev.mjs");
const DEV_SIGNALS = ["SIGINT", "SIGTERM"] as const;

type DevSignal = (typeof DEV_SIGNALS)[number];

interface ViteProcessHandle {
  readonly url: string;
  close(): Promise<void>;
}

class DevLogTee implements BackendLogger {
  private writeChain: Promise<void> = Promise.resolve();

  public log(event: BackendLogEvent): void {
    this.writeJson("pohunek-backend", event.level, event.event, { ...event }, true);
  }

  public process(component: string, level: "info" | "warn" | "error", message: string): void {
    this.writeJson(component, level, "process_output", { message }, false);
  }

  public lifecycle(level: "info" | "error", event: string, fields: Record<string, unknown> = {}): void {
    this.writeJson("pohunek-dev", level, event, fields, true);
  }

  public async close(): Promise<void> {
    await this.writeChain;
  }

  private writeJson(
    component: string,
    level: "info" | "warn" | "error",
    event: string,
    fields: Record<string, unknown>,
    writeToConsole: boolean,
  ): void {
    const line = JSON.stringify({
      timestamp: new Date().toISOString(),
      component,
      level,
      event,
      ...fields,
    });
    if (writeToConsole) {
      console.log(line);
    }
    this.writeChain = this.writeChain.then(() => appendFile(DEV_LOG_PATH, `${line}\n`));
  }
}

async function main(): Promise<void> {
  await mkdir(LOGS_DIR, { recursive: true });
  const logs = new DevLogTee();
  let stack: FixtureStackHandle | undefined;
  let vite: ViteProcessHandle | undefined;
  let shutdownTask: Promise<void> | undefined;

  const shutdown = (signal?: DevSignal): Promise<void> => {
    shutdownTask ??= (async (): Promise<void> => {
      logs.lifecycle("info", "dev_stack_stopping", signal === undefined ? {} : { signal });
      const results = await Promise.allSettled([
        vite?.close() ?? Promise.resolve(),
        stack?.close() ?? Promise.resolve(),
      ]);
      let closeFailed = false;
      for (const result of results) {
        if (result.status === "rejected") {
          closeFailed = true;
          logs.lifecycle("error", "dev_stack_close_failed", { error_class: errorClass(result.reason) });
        }
      }
      logs.lifecycle("info", "dev_stack_stopped");
      await logs.close();
      if (signal !== undefined) {
        process.exitCode = closeFailed ? FAILURE_EXIT_CODE : CLEAN_EXIT_CODE;
      }
    })();
    return shutdownTask;
  };

  try {
    stack = await startFixtureStack({ logger: logs });
    vite = await startViteProcess(stack.backend.url, logs);
    logs.lifecycle("info", "dev_stack_ready", {
      frontend_url: vite.url,
      backend_url: stack.backend.url,
      local_host: FIXTURE_LOCAL_HOST,
      peer_host: FIXTURE_PEER_HOST,
      log_path: DEV_LOG_PATH,
    });

    for (const signal of DEV_SIGNALS) {
      process.once(signal, (): void => {
        void shutdown(signal).catch((error: unknown): void => {
          process.exitCode = FAILURE_EXIT_CODE;
          console.error(JSON.stringify({
            timestamp: new Date().toISOString(),
            component: "pohunek-dev",
            level: "error",
            event: "dev_stack_shutdown_failed",
            error_class: errorClass(error),
          }));
        });
      });
    }
  } catch (error: unknown) {
    logs.lifecycle("error", "dev_stack_start_failed", { error_class: errorClass(error) });
    await shutdown();
    throw error;
  }
}

async function startViteProcess(backendUrl: string, logs: DevLogTee): Promise<ViteProcessHandle> {
  // Vite's WebSocket proxy requires Node net.Socket APIs that Bun 1.3 does not implement.
  const nodeExecutable = process.env[NODE_EXECUTABLE_ENV] ?? DEFAULT_NODE_EXECUTABLE;
  const child = spawn(nodeExecutable, [VITE_CHILD_PATH], {
    cwd: WEB_ROOT,
    env: {
      ...process.env,
      [VITE_BACKEND_URL_ENV]: backendUrl,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const stdout = createInterface({ input: child.stdout });
  const stderr = createInterface({ input: child.stderr });
  stdout.on("line", (line: string): void => {
    if (!line.startsWith(VITE_READY_PREFIX)) {
      logs.process("vite", "info", line);
    }
  });
  stderr.on("line", (line: string): void => {
    logs.process("vite", "error", line);
  });

  const url = await waitForViteReady(child, stdout);
  let closeTask: Promise<void> | undefined;
  return {
    url,
    close: (): Promise<void> => {
      closeTask ??= closeChildProcess(child);
      return closeTask;
    },
  };
}

function waitForViteReady(
  child: ChildProcess,
  stdout: ReturnType<typeof createInterface>,
): Promise<string> {
  return new Promise((resolve, reject): void => {
    const onLine = (line: string): void => {
      if (!line.startsWith(VITE_READY_PREFIX)) {
        return;
      }
      try {
        const ready = JSON.parse(line.slice(VITE_READY_PREFIX.length)) as { readonly url?: unknown };
        if (typeof ready.url !== "string") {
          throw new TypeError("Vite ready message did not contain a URL");
        }
        cleanup();
        resolve(ready.url);
      } catch (error: unknown) {
        cleanup();
        reject(asError(error));
      }
    };
    const onError = (error: Error): void => {
      cleanup();
      reject(error);
    };
    const onExit = (code: number | null, signal: NodeJS.Signals | null): void => {
      cleanup();
      reject(new Error(`Vite exited before becoming ready (code=${String(code)}, signal=${String(signal)})`));
    };
    const cleanup = (): void => {
      stdout.off("line", onLine);
      child.off("error", onError);
      child.off("exit", onExit);
    };

    stdout.on("line", onLine);
    child.once("error", onError);
    child.once("exit", onExit);
  });
}

function closeChildProcess(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject): void => {
    child.once("error", reject);
    child.once("exit", (): void => resolve());
    child.kill("SIGTERM");
  });
}

function errorClass(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

await main();
