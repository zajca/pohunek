import { describe, expect, test } from "bun:test";
import {
  DEFAULT_DISCOVER_INTERVAL_SECONDS,
  DEFAULT_LOG_MAX_FILES,
  DEFAULT_LOG_MAX_FILE_BYTES,
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

  test("file logging is off by default and carries documented limits when enabled", () => {
    expect(loadBackendConfig(baseEnv(), LINUX).logFiles).toBeUndefined();
    const config = loadBackendConfig({ ...baseEnv(), POHUNEK_BACKEND_LOG_DIR: "/var/log/pk" }, LINUX);
    expect(config.logFiles).toEqual({
      dir: "/var/log/pk",
      maxFileBytes: DEFAULT_LOG_MAX_FILE_BYTES,
      maxFiles: DEFAULT_LOG_MAX_FILES,
    });
    const tuned = loadBackendConfig({
      ...baseEnv(),
      POHUNEK_BACKEND_LOG_DIR: "/var/log/pk",
      POHUNEK_BACKEND_LOG_MAX_FILE_BYTES: "1048576",
      POHUNEK_BACKEND_LOG_MAX_FILES: "3",
    }, LINUX);
    expect(tuned.logFiles).toEqual({ dir: "/var/log/pk", maxFileBytes: 1_048_576, maxFiles: 3 });
  });

  test("invalid or orphaned log settings fail instead of defaulting", () => {
    for (const dir of ["", "relative/logs", "/var/../logs"]) {
      expectConfigError({ ...baseEnv(), POHUNEK_BACKEND_LOG_DIR: dir }, "POHUNEK_BACKEND_LOG_DIR");
    }
    for (const bad of ["0", "-1", "1.5", "many", ""]) {
      expectConfigError(
        { ...baseEnv(), POHUNEK_BACKEND_LOG_DIR: "/var/log/pk", POHUNEK_BACKEND_LOG_MAX_FILES: bad },
        "POHUNEK_BACKEND_LOG_MAX_FILES",
      );
    }
    expectConfigError(
      { ...baseEnv(), POHUNEK_BACKEND_LOG_MAX_FILE_BYTES: "1024" },
      "POHUNEK_BACKEND_LOG_MAX_FILE_BYTES",
    );
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
