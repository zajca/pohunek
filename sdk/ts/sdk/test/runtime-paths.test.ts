import { chmod, mkdir, mkdtemp, realpath, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";
import * as browserSdk from "@pohunek/sdk/browser";
import {
  RuntimePathError,
  SOCKET_PATH_MAX_BYTES,
  validateSocketPath,
  verifyDaemonRuntime,
} from "@pohunek/sdk";

const VARIABLE = "XDG_RUNTIME_DIR";
const OWNER_ONLY_MODE = 0o700;
const GROUP_READABLE_MODE = 0o750;
const SOCKET_FILE_NAME = "daemon.sock";

/** Runs `body` with a runtime directory of `mode` inside a scratch base that is always removed. */
async function withRuntimeDir(
  mode: number,
  body: (runtimeDir: string, base: string) => Promise<void> | void,
): Promise<void> {
  const base = await realpath(await mkdtemp(join(tmpdir(), "pohunek-sdk-runtime-")));
  try {
    const runtimeDir = join(base, "pohunek");
    await mkdir(runtimeDir);
    await chmod(runtimeDir, mode);
    await body(runtimeDir, base);
  } finally {
    await rm(base, { recursive: true, force: true });
  }
}

function assertNoFailure(action: () => unknown): void {
  action();
}

function failureOf(action: () => unknown): RuntimePathError {
  try {
    action();
  } catch (error: unknown) {
    if (error instanceof RuntimePathError) return error;
    throw error;
  }
  throw new Error("expected a RuntimePathError");
}

describe("runtime path entry points", () => {
  test("the node entry owns the resolver and the browser entry stays free of it", () => {
    expect("resolveRuntimeDir" in browserSdk).toBe(false);
    expect("verifyDaemonRuntime" in browserSdk).toBe(false);
    expect("RuntimePathError" in browserSdk).toBe(false);
  });
});

describe("validateSocketPath", () => {
  test("accepts an absolute path within the platform capacity", () => {
    assertNoFailure(() => validateSocketPath("linux", "/run/user/1000/pohunek/daemon.sock", VARIABLE));
  });

  test("rejects relative, parent-component and NUL-bearing paths", () => {
    expect(failureOf(() => validateSocketPath("linux", "run/daemon.sock", VARIABLE)).failure).toEqual({
      variant: "invalid_env",
      variable: VARIABLE,
      reason: "not_absolute",
    });
    expect(failureOf(() => validateSocketPath("linux", "/run/../daemon.sock", VARIABLE)).failure).toEqual({
      variant: "invalid_env",
      variable: VARIABLE,
      reason: "parent_component",
    });
    expect(failureOf(() => validateSocketPath("linux", "/run/\0daemon.sock", VARIABLE)).failure).toEqual({
      variant: "invalid_env",
      variable: VARIABLE,
      reason: "contains_nul",
    });
  });

  test("enforces the per-platform sun_path capacity at the byte boundary", () => {
    for (const platform of ["linux", "macos"] as const) {
      const limit = SOCKET_PATH_MAX_BYTES[platform];
      const atLimit = `/${"a".repeat(limit - 1)}`;
      assertNoFailure(() => validateSocketPath(platform, atLimit, VARIABLE));
      const caught = failureOf(() => validateSocketPath(platform, `${atLimit}a`, VARIABLE));
      expect(caught.failure.variant).toBe("socket_path_invalid");
    }
  });
});

describe("verifyDaemonRuntime", () => {
  const effectiveUid = process.geteuid?.() ?? -1;

  test("accepts an owner-only runtime directory without a socket", async () => {
    await withRuntimeDir(OWNER_ONLY_MODE, (runtimeDir) => {
      assertNoFailure(() =>
        verifyDaemonRuntime(runtimeDir, join(runtimeDir, SOCKET_FILE_NAME), effectiveUid, VARIABLE),
      );
    });
  });

  test("rejects a missing directory", async () => {
    await withRuntimeDir(OWNER_ONLY_MODE, (runtimeDir) => {
      const missing = join(runtimeDir, "absent");
      const caught = failureOf(() =>
        verifyDaemonRuntime(missing, join(missing, SOCKET_FILE_NAME), effectiveUid, VARIABLE),
      );
      expect(caught.failure.variant).toBe("runtime_dir_untrusted");
    });
  });

  test("rejects a directory with a mode other than 0700", async () => {
    await withRuntimeDir(GROUP_READABLE_MODE, (runtimeDir) => {
      const caught = failureOf(() =>
        verifyDaemonRuntime(runtimeDir, join(runtimeDir, SOCKET_FILE_NAME), effectiveUid, VARIABLE),
      );
      expect(caught.message).toContain("mode 0700");
    });
  });

  test("rejects a directory owned by a different user", async () => {
    await withRuntimeDir(OWNER_ONLY_MODE, (runtimeDir) => {
      const caught = failureOf(() =>
        verifyDaemonRuntime(runtimeDir, join(runtimeDir, SOCKET_FILE_NAME), effectiveUid + 1, VARIABLE),
      );
      expect(caught.message).toContain("not owned by the current user");
    });
  });

  test("rejects a symlinked runtime directory", async () => {
    await withRuntimeDir(OWNER_ONLY_MODE, async (runtimeDir, base) => {
      const link = join(base, "link");
      await symlink(runtimeDir, link);
      const caught = failureOf(() =>
        verifyDaemonRuntime(link, join(link, SOCKET_FILE_NAME), effectiveUid, VARIABLE),
      );
      expect(caught.message).toContain("not a real directory");
    });
  });

  test("rejects a non-socket entry at the socket path", async () => {
    await withRuntimeDir(OWNER_ONLY_MODE, async (runtimeDir) => {
      const socket = join(runtimeDir, SOCKET_FILE_NAME);
      await writeFile(socket, "");
      const caught = failureOf(() => verifyDaemonRuntime(runtimeDir, socket, effectiveUid, VARIABLE));
      expect(caught.message).toContain("is not a socket owned by the current user");
    });
  });
});
