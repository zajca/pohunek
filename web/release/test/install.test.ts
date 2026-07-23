import { execFile } from "node:child_process";
import { cp, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "bun:test";

const execFileAsync = promisify(execFile);
const RELEASE_DIR = join(dirname(fileURLToPath(import.meta.url)), "..");
const SERVICE_TEMPLATE = join(RELEASE_DIR, "..", "backend", "systemd", "pohunek-backend.service.in");

describe("web release installer", () => {
  test("installs atomically, removes stale assets, and preserves configuration", async () => {
    const root = await mkdtemp(join(tmpdir(), "pohunek-web-install-test-"));
    const archive = join(root, "archive");
    const dataHome = join(root, "data home");
    const configHome = join(root, "config home");
    const environment = {
      ...process.env,
      HOME: join(root, "home"),
      XDG_CONFIG_HOME: configHome,
      XDG_DATA_HOME: dataHome,
    };

    try {
      await createArchive(archive, "first build");
      await execFileAsync("sh", [join(archive, "install.sh")], { env: environment });

      const installDir = join(dataHome, "pohunek", "web");
      const configFile = join(configHome, "pohunek", "backend.env");
      const unitFile = join(configHome, "systemd", "user", "pohunek-backend.service");
      expect(await readFile(join(installDir, "frontend", "index.html"), "utf8")).toBe("first build");
      expect((await stat(join(installDir, "pohunek-web"))).mode & 0o777).toBe(0o755);
      expect((await stat(configFile)).mode & 0o777).toBe(0o600);

      const unit = await readFile(unitFile, "utf8");
      expect(unit).toContain(`ExecStart="${installDir}/pohunek-web"`);
      expect(unit).toContain(`EnvironmentFile="${configFile}"`);

      await writeFile(configFile, "POHUNEK_BACKEND_BIND_HOST=100.64.0.1\n", { mode: 0o644 });
      await writeFile(join(installDir, "frontend", "stale.js"), "stale");
      await writeFile(join(archive, "frontend", "index.html"), "second build");
      await execFileAsync("sh", [join(archive, "install.sh")], { env: environment });

      expect(await readFile(join(installDir, "frontend", "index.html"), "utf8")).toBe("second build");
      expect(await pathExists(join(installDir, "frontend", "stale.js"))).toBe(false);
      expect(await readFile(configFile, "utf8")).toBe(
        "POHUNEK_BACKEND_BIND_HOST=100.64.0.1\n",
      );
      expect((await stat(configFile)).mode & 0o777).toBe(0o600);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("fails before changing the installation when an archive is incomplete", async () => {
    const root = await mkdtemp(join(tmpdir(), "pohunek-web-install-invalid-"));
    const archive = join(root, "archive");
    await mkdir(archive, { recursive: true });
    await cp(join(RELEASE_DIR, "install.sh"), join(archive, "install.sh"));

    try {
      let thrown: unknown;
      try {
        await execFileAsync("sh", [join(archive, "install.sh")], {
          env: {
            ...process.env,
            HOME: join(root, "home"),
            XDG_CONFIG_HOME: join(root, "config"),
            XDG_DATA_HOME: join(root, "data"),
          },
        });
      } catch (error: unknown) {
        thrown = error;
      }

      expect(thrown).toBeInstanceOf(Error);
      expect(String((thrown as { readonly stderr?: unknown }).stderr)).toContain(
        "pohunek-web is missing or not executable",
      );
      expect(await pathExists(join(root, "data", "pohunek", "web"))).toBe(false);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

async function createArchive(path: string, indexContent: string): Promise<void> {
  await mkdir(join(path, "frontend"), { recursive: true });
  await writeFile(join(path, "pohunek-web"), "#!/usr/bin/env sh\nexit 0\n", { mode: 0o755 });
  await writeFile(join(path, "frontend", "index.html"), indexContent);
  await cp(SERVICE_TEMPLATE, join(path, "pohunek-backend.service.in"));
  await cp(join(RELEASE_DIR, "backend.env.example"), join(path, "backend.env.example"));
  await cp(join(RELEASE_DIR, "install.sh"), join(path, "install.sh"));
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch (error: unknown) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}
