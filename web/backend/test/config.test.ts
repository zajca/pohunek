import { spawn } from "node:child_process";
import { once } from "node:events";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, realpath, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import {
  DEFAULT_DISCOVER_INTERVAL_SECONDS,
  BackendConfigError,
  loadBackendConfig,
  type RuntimePathContext,
} from "@pohunek/backend";

const TEST_RUNTIME_DIR = "/tmp/pohunek-backend-config-runtime";
const TEST_STATIC_DIR = "/tmp/pohunek-backend-config-static";
const LINUX: RuntimePathContext = { platform: "linux", effectiveUid: 1000 };
const MACOS: RuntimePathContext = { platform: "macos", effectiveUid: 501 };

describe("backend configuration", () => {
  test("loads required values and derives defaults in one place", () => {
    const config = loadBackendConfig({
      POHUNEK_BACKEND_BIND_HOST: "100.64.0.10",
      POHUNEK_BACKEND_PORT: "8080",
      XDG_RUNTIME_DIR: TEST_RUNTIME_DIR,
    }, LINUX);

    expect(config.bindHost).toBe("100.64.0.10");
    expect(config.port).toBe(8080);
    expect(config.allowLoopbackBind).toBe(false);
    expect(config.daemonSocketPath).toBe(`${TEST_RUNTIME_DIR}/pohunek/daemon.sock`);
    expect(config.discoverIntervalSeconds).toBe(DEFAULT_DISCOVER_INTERVAL_SECONDS);
  });

  test("accepts explicit socket, interval, loopback, and assets", () => {
    const config = loadBackendConfig({
      POHUNEK_BACKEND_BIND_HOST: "127.0.0.1",
      POHUNEK_BACKEND_PORT: "0",
      POHUNEK_BACKEND_ALLOW_LOOPBACK: "yes",
      POHUNEK_BACKEND_DAEMON_SOCKET: "/tmp/custom-daemon.sock",
      POHUNEK_BACKEND_DISCOVER_INTERVAL: "0.05",
      POHUNEK_BACKEND_STATIC_DIR: TEST_STATIC_DIR,
    }, LINUX);

    expect(config.allowLoopbackBind).toBe(true);
    expect(config.daemonSocketPath).toBe("/tmp/custom-daemon.sock");
    expect(config.discoverIntervalSeconds).toBe(0.05);
    expect(config.staticAssetsDir).toBe(TEST_STATIC_DIR);
  });

  test("fails fast when required configuration is missing", () => {
    expectConfigError(
      { POHUNEK_BACKEND_PORT: "8080", XDG_RUNTIME_DIR: TEST_RUNTIME_DIR },
      "POHUNEK_BACKEND_BIND_HOST",
    );
    expectConfigError(
      { POHUNEK_BACKEND_BIND_HOST: "100.64.0.10", XDG_RUNTIME_DIR: TEST_RUNTIME_DIR },
      "POHUNEK_BACKEND_PORT",
    );
    expectConfigError(
      { POHUNEK_BACKEND_BIND_HOST: "100.64.0.10", POHUNEK_BACKEND_PORT: "8080" },
      "XDG_RUNTIME_DIR",
    );
  });

  test("macOS defaults the socket to the owner runtime directory and Linux does not", () => {
    const env = { POHUNEK_BACKEND_BIND_HOST: "100.64.0.10", POHUNEK_BACKEND_PORT: "8080" };
    expect(loadBackendConfig(env, MACOS).daemonSocketPath).toBe(
      "/private/tmp/pohunek-501/daemon.sock",
    );
    expect(loadBackendConfig({ ...env, XDG_RUNTIME_DIR: TEST_RUNTIME_DIR }, MACOS)
      .daemonSocketPath).toBe(`${TEST_RUNTIME_DIR}/pohunek/daemon.sock`);
    expectConfigError(env, "XDG_RUNTIME_DIR", LINUX);
  });

  test("an explicit runtime directory is validated, never treated as absent", () => {
    for (const bad of ["", "relative/run", "/run/../escape"]) {
      expectConfigError({ ...baseEnv(), XDG_RUNTIME_DIR: bad }, "XDG_RUNTIME_DIR", MACOS);
    }
  });

  test("a socket path above the native limit fails with the variable that caused it", () => {
    const longRuntime = `/${"r".repeat(120)}`;
    expectConfigError({ ...baseEnv(), XDG_RUNTIME_DIR: longRuntime }, "XDG_RUNTIME_DIR", LINUX);
    // 105 bytes: within Linux's 107-byte limit, over Darwin's 103.
    const runtime = `/${"r".repeat(84)}`;
    const config = loadBackendConfig({ ...baseEnv(), XDG_RUNTIME_DIR: runtime }, LINUX);
    expect(config.daemonSocketPath).toBe(`${runtime}/pohunek/daemon.sock`);
    expect(config.daemonSocketPath.length).toBe(105);
    expectConfigError({ ...baseEnv(), XDG_RUNTIME_DIR: runtime }, "XDG_RUNTIME_DIR", MACOS);
  });

  test("an explicit socket override follows the same path rules", () => {
    for (const bad of ["relative.sock", "/run/../daemon.sock", `/${"s".repeat(120)}`]) {
      expectConfigError(
        { ...baseEnv(), POHUNEK_BACKEND_DAEMON_SOCKET: bad },
        "POHUNEK_BACKEND_DAEMON_SOCKET",
        LINUX,
      );
    }
  });

  test("rejects invalid optional values instead of silently defaulting", () => {
    expectConfigError(
      { ...baseEnv(), POHUNEK_BACKEND_ALLOW_LOOPBACK: "sometimes" },
      "POHUNEK_BACKEND_ALLOW_LOOPBACK",
    );
    expectConfigError(
      { ...baseEnv(), POHUNEK_BACKEND_DISCOVER_INTERVAL: "0" },
      "POHUNEK_BACKEND_DISCOVER_INTERVAL",
    );
    expectConfigError(
      { ...baseEnv(), POHUNEK_BACKEND_STATIC_DIR: "" },
      "POHUNEK_BACKEND_STATIC_DIR",
    );
  });
});

describe("runtime directory trust", () => {
  test("a daemon runtime directory must be a real, owner-only directory of the current user", async () => {
    const base = await realpath(await mkdtemp(join(tmpdir(), "pk-trust-")));
    const uid = process.geteuid?.() ?? 0;
    const context: RuntimePathContext = { platform: "linux", effectiveUid: uid };
    const env = { ...baseEnv(), XDG_RUNTIME_DIR: base };
    const runtimeDir = join(base, "pohunek");
    try {
      // Absent: the connect reports an unreachable daemon instead.
      expect(loadBackendConfig(env, context).daemonSocketPath).toBe(join(runtimeDir, "daemon.sock"));

      await mkdir(runtimeDir, { mode: 0o700 });
      expect(loadBackendConfig(env, context).daemonSocketPath).toBe(join(runtimeDir, "daemon.sock"));

      await chmod(runtimeDir, 0o750);
      expectConfigError(env, "XDG_RUNTIME_DIR", context);
      await chmod(runtimeDir, 0o700);

      // Another user's directory: the same check with a different effective uid.
      expectConfigError(env, "XDG_RUNTIME_DIR", { platform: "linux", effectiveUid: uid + 1 });

      await rm(runtimeDir, { recursive: true });
      const elsewhere = join(base, "elsewhere");
      await mkdir(elsewhere, { mode: 0o700 });
      await symlink(elsewhere, runtimeDir);
      expectConfigError(env, "XDG_RUNTIME_DIR", context);
    } finally {
      await rm(base, { recursive: true, force: true });
    }
  });

  test("a present socket path that is not a socket of the current user is refused", async () => {
    const base = await realpath(await mkdtemp(join(tmpdir(), "pk-trust-")));
    const uid = process.geteuid?.() ?? 0;
    const context: RuntimePathContext = { platform: "linux", effectiveUid: uid };
    const env = { ...baseEnv(), XDG_RUNTIME_DIR: base };
    try {
      await mkdir(join(base, "pohunek"), { mode: 0o700 });
      await mkdir(join(base, "pohunek", "daemon.sock"));
      expectConfigError(env, "XDG_RUNTIME_DIR", context);
    } finally {
      await rm(base, { recursive: true, force: true });
    }
  });
});

describe("backend startup diagnostics", () => {
  test("a configuration error reaches the operator with the variable and the reason", async () => {
    const entrypoint = join(dirname(fileURLToPath(import.meta.url)), "../src/entrypoint.ts");
    const child = spawn(process.execPath, [entrypoint], {
      env: {
        PATH: process.env["PATH"] ?? "",
        POHUNEK_BACKEND_BIND_HOST: "100.64.0.10",
        POHUNEK_BACKEND_PORT: "8080",
        XDG_RUNTIME_DIR: "relative/run",
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string): void => {
      stderr += chunk;
    });
    child.stdout.resume();
    const [code] = (await once(child, "exit")) as [number | null];
    expect(code).toBe(1);
    expect(stderr.includes("XDG_RUNTIME_DIR must be an absolute path")).toBe(true);
  });
});

function baseEnv(): NodeJS.ProcessEnv {
  return {
    POHUNEK_BACKEND_BIND_HOST: "100.64.0.10",
    POHUNEK_BACKEND_PORT: "8080",
    XDG_RUNTIME_DIR: TEST_RUNTIME_DIR,
  };
}

function expectConfigError(
  env: NodeJS.ProcessEnv,
  variable: string,
  runtime: RuntimePathContext = LINUX,
): void {
  try {
    loadBackendConfig(env, runtime);
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(BackendConfigError);
    const configError = error as BackendConfigError;
    expect(configError.variable).toBe(variable);
    return;
  }
  throw new Error(`expected ${variable} configuration to fail`);
}
