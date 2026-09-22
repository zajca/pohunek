import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const FIXTURE_PATH = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../crates/paths/fixtures/runtime-paths.json",
);

const ENVIRONMENT_KEYS = [
  "XDG_RUNTIME_DIR",
  "XDG_DATA_HOME",
  "XDG_STATE_HOME",
  "XDG_CACHE_HOME",
  "XDG_CONFIG_HOME",
  "HOME",
] as const;

const DERIVED_PATH_KEYS = [
  "runtime_dir",
  "socket",
  "lock",
  "log_dir",
  "state_dir",
  "data_dir",
  "cache_dir",
  "config_home",
  "config_dir",
  "launcher_bin_dir",
  "sway_config_dir",
  "assistant_bundle_cache_dir",
  "assistant_runtime_dir",
  "worker_runtime_root",
  "worker_state_root",
  "worker_socket",
  "worker_journal",
  "host_state_dir",
  "host_identity_path",
  "host_approval_key_path",
  "host_governance_path",
  "host_state_lock_path",
] as const;

type EnvironmentKey = (typeof ENVIRONMENT_KEYS)[number];
type DerivedPathKey = (typeof DERIVED_PATH_KEYS)[number];
type Platform = "linux" | "macos";
type FixtureEnvironment = Partial<Record<EnvironmentKey, string>>;
type DerivedPaths = Record<DerivedPathKey, string>;

interface FixtureError {
  readonly variant: "missing_env" | "invalid_env";
  readonly var: string;
  readonly reason?: "empty" | "not_absolute" | "parent_component" | "contains_nul";
}

interface RuntimePathCase {
  readonly name: string;
  readonly platform: Platform;
  readonly effective_uid: number;
  readonly env: FixtureEnvironment;
  readonly expected?: DerivedPaths;
  readonly expected_runtime_dir?: string;
  readonly error?: FixtureError;
}

interface RuntimePathFixture {
  readonly version: number;
  readonly session_id: string;
  readonly worker_id: string;
  readonly assistant_id: string;
  readonly cases: RuntimePathCase[];
}

describe("shared runtime path contract", () => {
  test("is a complete, internally consistent cross-language fixture", () => {
    const fixture = readFixture();
    expect(fixture.version).toBe(1);
    expect(fixture.session_id).toBe("s-42");
    expect(fixture.worker_id.length > 0).toBe(true);
    expect(fixture.assistant_id.length > 0).toBe(true);

    const names = new Set<string>();
    for (const fixtureCase of fixture.cases) {
      validateCase(fixture, fixtureCase);
      expect(names.has(fixtureCase.name)).toBe(false);
      names.add(fixtureCase.name);
    }

    for (const required of [
      "linux_explicit_overrides",
      "macos_runtime_and_home_defaults",
      "macos_explicit_runtime_alternate_uid",
      "macos_default_alternate_uid",
      "linux_missing_runtime",
      "missing_home_for_durable_defaults",
      "empty_explicit_runtime",
      "relative_explicit_state_home",
    ]) {
      expect(names.has(required)).toBe(true);
    }
  });
});

function readFixture(): RuntimePathFixture {
  const value: unknown = JSON.parse(readFileSync(FIXTURE_PATH, "utf8"));
  const record = asRecord(value, "fixture");
  expect(Object.keys(record).sort()).toEqual([
    "assistant_id",
    "cases",
    "session_id",
    "version",
    "worker_id",
  ]);
  expect(Array.isArray(record.cases)).toBe(true);
  return value as RuntimePathFixture;
}

function validateCase(fixture: RuntimePathFixture, fixtureCase: RuntimePathCase): void {
  expect(fixtureCase.name.length > 0).toBe(true);
  expect(["linux", "macos"].includes(fixtureCase.platform)).toBe(true);
  expect(Number.isInteger(fixtureCase.effective_uid)).toBe(true);
  expect(fixtureCase.effective_uid >= 0).toBe(true);
  expect(fixtureCase.effective_uid <= 4_294_967_295).toBe(true);
  validateEnvironment(fixtureCase.env);

  const outcomes = [fixtureCase.expected, fixtureCase.expected_runtime_dir, fixtureCase.error]
    .filter((value) => value !== undefined);
  expect(outcomes.length).toBe(1);

  if (fixtureCase.expected !== undefined) {
    expect(Object.keys(fixtureCase.expected).sort()).toEqual([...DERIVED_PATH_KEYS].sort());
    expect(fixtureCase.expected).toEqual(
      deriveCompletePaths(fixture, fixtureCase.platform, fixtureCase.effective_uid, fixtureCase.env),
    );
  } else if (fixtureCase.expected_runtime_dir !== undefined) {
    expect(fixtureCase.expected_runtime_dir).toBe(
      deriveRuntimeDir(fixtureCase.platform, fixtureCase.effective_uid, fixtureCase.env),
    );
  } else {
    const expectedError = validateExpectedError(fixtureCase.error);
    let actualError: Error | undefined;
    try {
      deriveCompletePaths(fixture, fixtureCase.platform, fixtureCase.effective_uid, fixtureCase.env);
    } catch (error) {
      expect(error).toBeInstanceOf(Error);
      if (error instanceof Error) actualError = error;
    }
    expect(actualError?.message).toBe(expectedError.message);
  }
}

function validateEnvironment(env: FixtureEnvironment): void {
  const record = asRecord(env, "environment");
  for (const key of Object.keys(record)) {
    expect(ENVIRONMENT_KEYS.includes(key as EnvironmentKey)).toBe(true);
    expect(typeof record[key]).toBe("string");
  }
}

function deriveCompletePaths(
  fixture: RuntimePathFixture,
  platform: Platform,
  effectiveUid: number,
  env: FixtureEnvironment,
): DerivedPaths {
  const runtimeDir = deriveRuntimeDir(platform, effectiveUid, env);
  const dataHome = xdgOrHome(env, "XDG_DATA_HOME", ".local/share");
  const stateHome = xdgOrHome(env, "XDG_STATE_HOME", ".local/state");
  const cacheHome = xdgOrHome(env, "XDG_CACHE_HOME", ".cache");
  const configHome = xdgOrHome(env, "XDG_CONFIG_HOME", ".config");
  const dataDir = posix.join(dataHome, "pohunek");
  const stateDir = posix.join(stateHome, "pohunek");
  const cacheDir = posix.join(cacheHome, "pohunek");
  const workerRuntimeRoot = posix.join(runtimeDir, "workers");
  const workerStateRoot = posix.join(stateDir, "workers");
  const hostStateDir = posix.join(stateDir, "host");

  return {
    runtime_dir: runtimeDir,
    socket: posix.join(runtimeDir, "daemon.sock"),
    lock: posix.join(runtimeDir, "daemon.lock"),
    log_dir: posix.join(stateDir, "logs"),
    state_dir: stateDir,
    data_dir: dataDir,
    cache_dir: cacheDir,
    config_home: configHome,
    config_dir: posix.join(configHome, "pohunek"),
    launcher_bin_dir: posix.join(dataDir, "bin"),
    sway_config_dir: posix.join(configHome, "sway"),
    assistant_bundle_cache_dir: posix.join(cacheDir, "knowledge"),
    assistant_runtime_dir: posix.join(runtimeDir, "assistant", fixture.assistant_id),
    worker_runtime_root: workerRuntimeRoot,
    worker_state_root: workerStateRoot,
    worker_socket: posix.join(workerRuntimeRoot, fixture.session_id, "control.sock"),
    worker_journal: posix.join(
      workerStateRoot,
      fixture.session_id,
      `${fixture.worker_id}.json`,
    ),
    host_state_dir: hostStateDir,
    host_identity_path: posix.join(hostStateDir, "identity.json"),
    host_approval_key_path: posix.join(hostStateDir, "approval.key"),
    host_governance_path: posix.join(hostStateDir, "governance.json"),
    host_state_lock_path: posix.join(hostStateDir, "state.lock"),
  };
}

function deriveRuntimeDir(
  platform: Platform,
  effectiveUid: number,
  env: FixtureEnvironment,
): string {
  if (env.XDG_RUNTIME_DIR !== undefined) {
    return posix.join(validateBasePath(env.XDG_RUNTIME_DIR, "XDG_RUNTIME_DIR"), "pohunek");
  }
  if (platform === "macos") {
    return `/private/tmp/pohunek-${effectiveUid}`;
  }
  throw contractError({ variant: "missing_env", var: "XDG_RUNTIME_DIR" });
}

function validateExpectedError(error: FixtureError | undefined): Error {
  if (error === undefined) throw new Error("fixture case has no outcome");
  expect(["missing_env", "invalid_env"].includes(error.variant)).toBe(true);
  expect(error.var.length > 0).toBe(true);
  if (error.variant === "missing_env") {
    expect(error.reason).toBeUndefined();
  } else {
    expect(
      ["empty", "not_absolute", "parent_component", "contains_nul"].includes(
        error.reason ?? "",
      ),
    ).toBe(true);
  }
  return contractError(error);
}

function xdgOrHome(
  env: FixtureEnvironment,
  key: Exclude<EnvironmentKey, "XDG_RUNTIME_DIR" | "HOME">,
  fallback: string,
): string {
  const explicit = env[key];
  if (explicit !== undefined) return validateBasePath(explicit, key);
  if (env.HOME === undefined) {
    throw contractError({ variant: "missing_env", var: `${key} or HOME` });
  }
  return posix.join(validateBasePath(env.HOME, "HOME"), fallback);
}

function validateBasePath(value: string, variable: string): string {
  if (value.length === 0) {
    throw contractError({ variant: "invalid_env", var: variable, reason: "empty" });
  }
  if (!value.startsWith("/")) {
    throw contractError({ variant: "invalid_env", var: variable, reason: "not_absolute" });
  }
  if (value.split("/").includes("..")) {
    throw contractError({ variant: "invalid_env", var: variable, reason: "parent_component" });
  }
  if (value.includes("\0")) {
    throw contractError({ variant: "invalid_env", var: variable, reason: "contains_nul" });
  }
  return value;
}

function contractError(error: FixtureError): Error {
  return new Error(`${error.variant}:${error.var}:${error.reason ?? ""}`);
}

function asRecord(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}
