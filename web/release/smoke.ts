import { mkdtemp, rm } from "node:fs/promises";
import { once } from "node:events";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { spawn, type ChildProcess } from "node:child_process";
import { startFixtureDaemon, type FixtureDaemonHandle } from "@pohunek/testkit";

const LOOPBACK_HOST = "127.0.0.1";
// A packaged backend should report readiness quickly even on a cold CI runner.
const STARTUP_TIMEOUT_MILLISECONDS = 10_000;
const EXPECTED_HTML_MARKER = "<!doctype html>";

interface ReadyEvent {
  readonly event?: unknown;
  readonly lifecycle?: unknown;
  readonly url?: unknown;
}

async function main(): Promise<void> {
  const executable = process.argv[2];
  const staticAssetsDir = process.argv[3];
  if (executable === undefined || staticAssetsDir === undefined) {
    throw new Error("usage: bun run release/smoke.ts <backend-executable> <static-assets-dir>");
  }

  const root = await mkdtemp(join(tmpdir(), "pohunek-web-release-smoke-"));
  const socketPath = join(root, "daemon.sock");
  let daemon: FixtureDaemonHandle | undefined;
  let backend: ChildProcess | undefined;

  try {
    daemon = await startFixtureDaemon({ listen: { unixSocketPath: socketPath } });
    backend = spawn(resolve(executable), [], {
      env: {
        ...process.env,
        POHUNEK_BACKEND_ALLOW_LOOPBACK: "true",
        POHUNEK_BACKEND_BIND_HOST: LOOPBACK_HOST,
        POHUNEK_BACKEND_DAEMON_SOCKET: socketPath,
        POHUNEK_BACKEND_PORT: "0",
        POHUNEK_BACKEND_STATIC_DIR: resolve(staticAssetsDir),
      },
      stdio: ["ignore", "pipe", "pipe"],
    });

    const url = await waitForReadyUrl(backend);
    const index = await fetch(`${url}/`);
    if (!index.ok || !(await index.text()).toLowerCase().includes(EXPECTED_HTML_MARKER)) {
      throw new Error(`packaged backend did not serve the compiled SPA (HTTP ${index.status})`);
    }

    const hosts = await fetch(`${url}/api/hosts`);
    if (!hosts.ok) {
      throw new Error(`packaged backend host discovery failed (HTTP ${hosts.status})`);
    }
    const payload = await hosts.json() as readonly { readonly host?: unknown }[];
    if (!payload.some((host) => host.host === "local")) {
      throw new Error("packaged backend did not discover its local fixture daemon");
    }
  } finally {
    await stopChild(backend);
    await daemon?.close();
    await rm(root, { recursive: true, force: true });
  }
}

function waitForReadyUrl(child: ChildProcess): Promise<string> {
  return new Promise((resolveReady, rejectReady): void => {
    const stdoutStream = child.stdout;
    const stderrStream = child.stderr;
    if (stdoutStream === null || stderrStream === null) {
      rejectReady(new Error("packaged backend smoke test requires piped stdout and stderr"));
      return;
    }

    const stdout = createInterface({ input: stdoutStream });
    const stderr: string[] = [];
    stderrStream.setEncoding("utf8");
    stderrStream.on("data", (chunk: string): void => {
      stderr.push(chunk);
    });

    const timeout = setTimeout((): void => {
      cleanup();
      rejectReady(new Error("packaged backend did not report readiness before the timeout"));
    }, STARTUP_TIMEOUT_MILLISECONDS);

    const cleanup = (): void => {
      clearTimeout(timeout);
      stdout.close();
      child.off("exit", onExit);
    };
    const onExit = (code: number | null): void => {
      cleanup();
      rejectReady(
        new Error(`packaged backend exited before readiness (${code ?? "signal"}): ${stderr.join("")}`),
      );
    };

    child.once("exit", onExit);
    stdout.on("line", (line: string): void => {
      let event: ReadyEvent;
      try {
        event = JSON.parse(line) as ReadyEvent;
      } catch {
        return;
      }
      if (
        event.event === "backend_server"
        && event.lifecycle === "listening"
        && typeof event.url === "string"
      ) {
        cleanup();
        resolveReady(event.url);
      }
    });
  });
}

async function stopChild(child: ChildProcess | undefined): Promise<void> {
  if (child === undefined || child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  const exit = once(child, "exit");
  child.kill("SIGTERM");
  await exit;
}

void main().catch((error: unknown): void => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
