import { describe, expect, test } from "bun:test";
import {
  DEFAULT_DISCOVER_INTERVAL_SECONDS,
  BackendConfigError,
  loadBackendConfig,
} from "@pohunek/backend";

const TEST_RUNTIME_DIR = "/tmp/pohunek-backend-config-runtime";
const TEST_STATIC_DIR = "/tmp/pohunek-backend-config-static";

describe("backend configuration", () => {
  test("loads required values and derives defaults in one place", () => {
    const config = loadBackendConfig({
      POHUNEK_BACKEND_BIND_HOST: "100.64.0.10",
      POHUNEK_BACKEND_PORT: "8080",
      XDG_RUNTIME_DIR: TEST_RUNTIME_DIR,
    });

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
    });

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

function expectConfigError(env: NodeJS.ProcessEnv, variable: string): void {
  try {
    loadBackendConfig(env);
  } catch (error: unknown) {
    expect(error).toBeInstanceOf(BackendConfigError);
    const configError = error as BackendConfigError;
    expect(configError.variable).toBe(variable);
    return;
  }
  throw new Error(`expected ${variable} configuration to fail`);
}
