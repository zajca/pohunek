import { describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, readdir, realpath, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  LOG_FILE_NAME,
  LogFileError,
  rotatingFileLogger,
  startBackendFromEnv,
  type BackendLogEvent,
} from "@pohunek/backend";
import { createFixtureRoot, startFixtureDaemon } from "@pohunek/testkit";

const SMALL_FILE_BYTES = 400;

describe("rotating backend log files", () => {
  test("writes one JSON object per line into an owner-private file", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      const logger = rotatingFileLogger({ dir, maxFileBytes: 4096, maxFiles: 3 });
      logger.log({ level: "info", event: "backend_server", lifecycle: "listening" });
      logger.log({ level: "error", event: "backend_startup", error_class: "Error" });

      const lines = (await readFile(join(dir, LOG_FILE_NAME), "utf8")).trimEnd().split("\n");
      expect(lines.length).toBe(2);
      const first = JSON.parse(lines[0] ?? "") as Record<string, unknown>;
      expect(first["component"]).toBe("pohunek-backend");
      expect(first["event"]).toBe("backend_server");
      expect(typeof first["timestamp"]).toBe("string");
      expect((await stat(dir)).mode & 0o777).toBe(0o700);
      expect((await stat(join(dir, LOG_FILE_NAME))).mode & 0o777).toBe(0o600);
    });
  });

  test("rotates before the active file exceeds its bound and keeps the newest files", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      const logger = rotatingFileLogger({ dir, maxFileBytes: SMALL_FILE_BYTES, maxFiles: 3 });
      for (let index = 0; index < 40; index += 1) {
        logger.log({ level: "info", event: `event_${String(index).padStart(2, "0")}` });
      }
      const names = (await readdir(dir)).sort();
      expect(names).toEqual([LOG_FILE_NAME, `${LOG_FILE_NAME}.1`, `${LOG_FILE_NAME}.2`]);
      for (const name of names) {
        expect((await stat(join(dir, name))).size <= SMALL_FILE_BYTES).toBe(true);
        expect((await stat(join(dir, name))).mode & 0o777).toBe(0o600);
      }
      // The newest event is in the active file; older ones are in higher suffixes.
      const active = await readFile(join(dir, LOG_FILE_NAME), "utf8");
      expect(active.includes("event_39")).toBe(true);
      const newestRotated = await readFile(join(dir, `${LOG_FILE_NAME}.1`), "utf8");
      const oldestRotated = await readFile(join(dir, `${LOG_FILE_NAME}.2`), "utf8");
      expect(newestRotated.includes("event_39")).toBe(false);
      expect(oldestRotated.includes("event_00")).toBe(false);
      expect(newestRotated.indexOf("event_") > -1 && oldestRotated.indexOf("event_") > -1).toBe(true);
    });
  });

  test("one file only truncates by replacement instead of growing", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      const logger = rotatingFileLogger({ dir, maxFileBytes: SMALL_FILE_BYTES, maxFiles: 1 });
      for (let index = 0; index < 30; index += 1) {
        logger.log({ level: "info", event: `event_${String(index)}` });
      }
      expect(await readdir(dir)).toEqual([LOG_FILE_NAME]);
      expect((await stat(join(dir, LOG_FILE_NAME))).size <= SMALL_FILE_BYTES).toBe(true);
    });
  });

  test("an event above the file bound is replaced by a fixed notice", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      const logger = rotatingFileLogger({ dir, maxFileBytes: SMALL_FILE_BYTES, maxFiles: 2 });
      logger.log({ level: "info", event: "x".repeat(SMALL_FILE_BYTES * 2) });
      const content = await readFile(join(dir, LOG_FILE_NAME), "utf8");
      expect(content.includes("log_event_dropped")).toBe(true);
      expect(content.includes("xxxx")).toBe(false);
    });
  });

  test("an existing file is appended to and rotated files beyond the limit are pruned", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      await mkdir(dir, { mode: 0o700 });
      await writeFile(join(dir, LOG_FILE_NAME), '{"previous":true}\n', { mode: 0o600 });
      await writeFile(join(dir, `${LOG_FILE_NAME}.5`), "stale\n", { mode: 0o600 });
      await writeFile(join(dir, "unrelated.txt"), "keep\n", { mode: 0o600 });
      const logger = rotatingFileLogger({ dir, maxFileBytes: 4096, maxFiles: 2 });
      logger.log({ level: "info", event: "after_restart" });
      const names = (await readdir(dir)).sort();
      expect(names).toEqual([LOG_FILE_NAME, "unrelated.txt"]);
      const content = await readFile(join(dir, LOG_FILE_NAME), "utf8");
      expect(content.startsWith('{"previous":true}\n')).toBe(true);
      expect(content.includes("after_restart")).toBe(true);
    });
  });

  test("refuses a symlinked directory, a symlinked file, a shared directory and bad limits", async () => {
    await withRoot(async (root) => {
      const real = join(root, "real");
      await mkdir(real, { mode: 0o700 });
      await symlink(real, join(root, "link"));
      expectLogFileError(() => rotatingFileLogger({ dir: join(root, "link"), maxFileBytes: 4096, maxFiles: 2 }));

      const shared = join(root, "shared");
      await mkdir(shared, { mode: 0o755 });
      expectLogFileError(() => rotatingFileLogger({ dir: shared, maxFileBytes: 4096, maxFiles: 2 }));

      const linked = join(root, "linked-file");
      await mkdir(linked, { mode: 0o700 });
      await writeFile(join(root, "elsewhere"), "", { mode: 0o600 });
      await symlink(join(root, "elsewhere"), join(linked, LOG_FILE_NAME));
      let failed = false;
      try {
        rotatingFileLogger({ dir: linked, maxFileBytes: 4096, maxFiles: 2 });
      } catch {
        failed = true;
      }
      expect(failed).toBe(true);
      expect(await readFile(join(root, "elsewhere"), "utf8")).toBe("");

      expectLogFileError(() => rotatingFileLogger({ dir: join(root, "limits"), maxFileBytes: 0, maxFiles: 2 }));
      expectLogFileError(() => rotatingFileLogger({ dir: join(root, "limits"), maxFileBytes: 10, maxFiles: 0 }));
    });
  });
});

describe("rotating log file failure handling", () => {
  test("a failing rotation reaches the fallback instead of the caller and recovers", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      const received: BackendLogEvent[] = [];
      const logger = rotatingFileLogger({
        dir,
        maxFileBytes: SMALL_FILE_BYTES,
        maxFiles: 3,
        fallback: { log: (event): void => void received.push(event) },
      });
      logger.log({ level: "info", event: "first" });
      // A read-only directory makes the next rotation fail like a full disk would.
      await chmod(dir, 0o500);
      try {
        for (let index = 0; index < 10; index += 1) {
          logger.log({ level: "info", event: `during_failure_${String(index)}` });
        }
      } finally {
        await chmod(dir, 0o700);
      }
      const failures = received.filter((event) => event.event === "log_file_failed");
      expect(failures.length).toBe(1);
      expect(received.some((event) => event.event === "during_failure_9")).toBe(true);

      logger.log({ level: "info", event: "after_recovery" });
      const names = await readdir(dir);
      let found = false;
      for (const name of names) {
        if ((await readFile(join(dir, name), "utf8")).includes("after_recovery")) found = true;
      }
      expect(found).toBe(true);
    });
  });

  test("events after close go to the fallback and close is idempotent", async () => {
    await withRoot((root) => {
      const received: BackendLogEvent[] = [];
      const logger = rotatingFileLogger({
        dir: join(root, "logs"),
        maxFileBytes: 4096,
        maxFiles: 2,
        fallback: { log: (event): void => void received.push(event) },
      });
      logger.close();
      logger.close();
      logger.log({ level: "info", event: "late" });
      expect(received.map((event) => event.event)).toEqual(["late"]);
      return Promise.resolve();
    });
  });

  test("files left above the bound or with loose modes are repaired at open", async () => {
    await withRoot(async (root) => {
      const dir = join(root, "logs");
      await mkdir(dir, { mode: 0o700 });
      const big = "x".repeat(SMALL_FILE_BYTES * 3);
      await writeFile(join(dir, LOG_FILE_NAME), big, { mode: 0o644 });
      await chmod(join(dir, LOG_FILE_NAME), 0o644);
      await writeFile(join(dir, `${LOG_FILE_NAME}.1`), big, { mode: 0o600 });
      await writeFile(join(dir, `${LOG_FILE_NAME}.2`), "small\n", { mode: 0o644 });
      await chmod(join(dir, `${LOG_FILE_NAME}.2`), 0o644);
      const logger = rotatingFileLogger({ dir, maxFileBytes: SMALL_FILE_BYTES, maxFiles: 3 });
      logger.close();

      expect((await stat(join(dir, LOG_FILE_NAME))).size).toBe(0);
      expect((await stat(join(dir, LOG_FILE_NAME))).mode & 0o777).toBe(0o600);
      expect((await readdir(dir)).sort()).toEqual([LOG_FILE_NAME, `${LOG_FILE_NAME}.2`]);
      expect((await stat(join(dir, `${LOG_FILE_NAME}.2`))).mode & 0o777).toBe(0o600);
    });
  });
});

describe("backend log destination", () => {
  test("a configured log directory receives the backend lifecycle events", async () => {
    const root = await createFixtureRoot("pk-log-");
    try {
      const socketPath = join(root, "daemon.sock");
      const logDir = join(root, "logs");
      const daemon = await startFixtureDaemon({ listen: { unixSocketPath: socketPath } });
      try {
        const backend = await startBackendFromEnv({
          POHUNEK_BACKEND_BIND_HOST: "127.0.0.1",
          POHUNEK_BACKEND_PORT: "0",
          POHUNEK_BACKEND_ALLOW_LOOPBACK: "1",
          POHUNEK_BACKEND_DAEMON_SOCKET: socketPath,
          POHUNEK_BACKEND_STATIC_DIR: root,
          POHUNEK_BACKEND_LOG_DIR: logDir,
        });
        await backend.close();
      } finally {
        await daemon.close();
      }
      const lines = (await readFile(join(logDir, LOG_FILE_NAME), "utf8")).trimEnd().split("\n");
      const lifecycles = lines.map((line) => (JSON.parse(line) as { lifecycle?: string }).lifecycle);
      expect(lifecycles.includes("listening")).toBe(true);
      expect(lifecycles.includes("closed")).toBe(true);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

function expectLogFileError(action: () => unknown): void {
  let failure: unknown;
  try {
    action();
  } catch (error: unknown) {
    failure = error;
  }
  expect(failure).toBeInstanceOf(LogFileError);
}

async function withRoot(run: (root: string) => Promise<void>): Promise<void> {
  const root = await realpath(await mkdtemp(join(tmpdir(), "pohunek-log-files-")));
  try {
    await run(root);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
