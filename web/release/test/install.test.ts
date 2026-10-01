import { execFile } from "node:child_process";
import { chmod, cp, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "bun:test";

const execFileAsync = promisify(execFile);
const RELEASE_DIR = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPOSITORY_ROOT = join(RELEASE_DIR, "..", "..");
const SERVICE_TEMPLATE = join(RELEASE_DIR, "..", "backend", "systemd", "pohunek-backend.service.in");
const WRITE_MANIFEST = join(REPOSITORY_ROOT, "packaging", "write-manifest");
const VERIFY_ARCHIVE = join(REPOSITORY_ROOT, "packaging", "verify-archive");

const LINUX_TARGET = "x86_64-unknown-linux-gnu";
const MACOS_TARGET = "aarch64-apple-darwin";
const MINIMUM_MACOS = "14.0";
const NAMESPACE = "abc123def456";
const AGENT_LABEL = `io.github.zajca.pohunek.${NAMESPACE}.backend`;
const FAKE_UID = "501";

// Stand-in for plutil: builds a real XML property list with plistlib from the
// same -create/-insert/-lint calls the installer makes.
const FAKE_PLUTIL = `#!/usr/bin/env python3
import plistlib, sys

args = sys.argv[1:]
if args[0] == "-create":
    with open(args[2], "wb") as handle:
        plistlib.dump({}, handle)
elif args[0] == "-lint":
    with open(args[1], "rb") as handle:
        plistlib.load(handle)
elif args[0] == "-insert":
    keypath, kind, file = args[1], args[2], args[-1]
    value = {"-string": lambda: args[3], "-array": lambda: [], "-dictionary": lambda: {},
             "-bool": lambda: args[3] == "true", "-integer": lambda: int(args[3])}[kind]()
    with open(file, "rb") as handle:
        document = plistlib.load(handle)
    parts = keypath.split(".")
    target = document
    for part in parts[:-1]:
        target = target[int(part)] if isinstance(target, list) else target[part]
    last = parts[-1]
    if isinstance(target, list):
        target.insert(int(last), value)
    else:
        target[last] = value
    with open(file, "wb") as handle:
        plistlib.dump(document, handle)
else:
    sys.exit("unexpected plutil call: " + " ".join(args))
`;

const FAKE_LAUNCHCTL = `#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LAUNCHCTL_LOG"
case "$1" in
  print) [ -f "$FAKE_LAUNCHCTL_LOADED" ] ;;
  bootstrap) : > "$FAKE_LAUNCHCTL_LOADED" ;;
  bootout) rm -f "$FAKE_LAUNCHCTL_LOADED" ;;
  *) exit 64 ;;
esac
`;

interface Harness {
  readonly root: string;
  readonly archive: string;
  readonly home: string;
  readonly dataHome: string;
  readonly configHome: string;
  readonly stateHome: string;
  readonly commands: string;
  readonly launchctlLog: string;
  readonly loadedMarker: string;
  readonly environment: NodeJS.ProcessEnv;
}

interface RunResult {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

describe("web release installer on Linux", () => {
  test("installs atomically, removes stale assets, and preserves configuration", async () => {
    const harness = await createHarness("linux");
    try {
      await createArchive(harness, "first build", LINUX_TARGET);
      expectSuccess(await install(harness));

      const installDir = join(harness.dataHome, "pohunek", "web");
      const configFile = join(harness.configHome, "pohunek", "backend.env");
      const unitFile = join(harness.configHome, "systemd", "user", "pohunek-backend.service");
      expect(await readFile(join(installDir, "frontend", "index.html"), "utf8")).toBe("first build");
      expect((await stat(join(installDir, "pohunek-web"))).mode & 0o777).toBe(0o755);
      expect((await stat(configFile)).mode & 0o777).toBe(0o600);

      const unit = await readFile(unitFile, "utf8");
      expect(unit).toContain(`ExecStart="${installDir}/pohunek-web"`);
      expect(unit).toContain(`EnvironmentFile=${configFile.replaceAll(" ", "\\x20")}`);

      await writeFile(configFile, "POHUNEK_BACKEND_BIND_HOST=100.64.0.1\n", { mode: 0o644 });
      await writeFile(join(installDir, "frontend", "stale.js"), "stale");
      await writeFile(join(harness.archive, "frontend", "index.html"), "second build");
      await sealManifest(harness.archive, LINUX_TARGET);
      expectSuccess(await install(harness));

      expect(await readFile(join(installDir, "frontend", "index.html"), "utf8")).toBe("second build");
      expect(await pathExists(join(installDir, "frontend", "stale.js"))).toBe(false);
      expect(await readFile(configFile, "utf8")).toBe("POHUNEK_BACKEND_BIND_HOST=100.64.0.1\n");
      expect((await stat(configFile)).mode & 0o777).toBe(0o600);
      expect(await pathExists(join(harness.dataHome, "pohunek", ".web-install.lock"))).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("fails before changing the installation when an archive is incomplete", async () => {
    const harness = await createHarness("linux");
    try {
      await mkdir(join(harness.archive, "packaging"), { recursive: true });
      await cp(join(RELEASE_DIR, "install.sh"), join(harness.archive, "install.sh"));
      await cp(VERIFY_ARCHIVE, join(harness.archive, "packaging", "verify-archive"));
      await writeFile(join(harness.archive, "marker"), "x");
      await execFileAsync("chmod", ["-R", "go-w", harness.archive]);
      await sealManifest(harness.archive, LINUX_TARGET);

      const result = await install(harness);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toContain("does not list the required binary: pohunek-web");
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("refuses a modified or manifest-less archive before anything is created", async () => {
    const harness = await createHarness("linux");
    try {
      await createArchive(harness, "build", LINUX_TARGET);
      await writeFile(join(harness.archive, "frontend", "index.html"), "tampered");
      const tampered = await install(harness);
      expect(tampered.code).not.toBe(0);
      expect(tampered.stderr).toContain("digest mismatch");
      expect(tampered.stderr).toContain("nothing was changed");
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);

      await rm(join(harness.archive, "MANIFEST"));
      const bare = await install(harness);
      expect(bare.code).not.toBe(0);
      expect(bare.stderr).toContain("no MANIFEST");
      expect(await pathExists(join(harness.configHome, "pohunek"))).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("refuses an archive built for another target and the daemon archive", async () => {
    const harness = await createHarness("linux");
    try {
      await createArchive(harness, "build", MACOS_TARGET);
      const wrongTarget = await install(harness);
      expect(wrongTarget.code).not.toBe(0);
      expect(wrongTarget.stderr).toContain("is built for aarch64-apple-darwin");

      await createArchive(harness, "build", LINUX_TARGET, "daemon");
      const wrongComponent = await install(harness);
      expect(wrongComponent.code).not.toBe(0);
      expect(wrongComponent.stderr).toContain("this is the daemon archive");
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("refuses a second installer while one runs and takes over a stale lock", async () => {
    const harness = await createHarness("linux");
    try {
      await createArchive(harness, "build", LINUX_TARGET);
      const lock = join(harness.dataHome, "pohunek", ".web-install.lock");
      await mkdir(lock, { recursive: true });
      // The test process itself is a live holder.
      await writeFile(join(lock, "pid"), `${process.pid}\n`);
      const busy = await install(harness);
      expect(busy.code).not.toBe(0);
      expect(busy.stderr).toContain("another web installer is running");
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);

      await writeFile(join(lock, "pid"), "2147483646\n");
      expectSuccess(await install(harness));
      expect(await pathExists(lock)).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("--uninstall is macOS-only", async () => {
    const harness = await createHarness("linux");
    try {
      await createArchive(harness, "build", LINUX_TARGET);
      const result = await install(harness, ["--uninstall"]);
      expect(result.code).toBe(2);
      expect(result.stderr).toContain("macOS only");
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });
});

describe("web release installer on macOS", () => {
  test("registers an agent whose property list carries serialized values only", async () => {
    const harness = await createHarness("macos");
    try {
      await createArchive(harness, "mac build", MACOS_TARGET);
      const hostile = join(harness.root, "sock et's $HOME", "daemon.sock");
      const configFile = join(harness.configHome, "pohunek", "backend.env");
      await mkdir(dirname(configFile), { recursive: true });
      await writeFile(
        configFile,
        `# comment\nPOHUNEK_BACKEND_BIND_HOST=100.64.0.7\nPOHUNEK_BACKEND_PORT="8443"\nPOHUNEK_BACKEND_DAEMON_SOCKET=${hostile}\nPOHUNEK_BACKEND_ALLOW_LOOPBACK=\n`,
        { mode: 0o644 },
      );
      const runtime = join(harness.root, "run");
      const result = await install(harness, [], {
        XDG_RUNTIME_DIR: runtime,
        SECRET_TOKEN: "must-not-reach-the-agent",
      });
      expectSuccess(result);

      const installDir = join(harness.dataHome, "pohunek", "web");
      const agentFile = join(harness.home, "Library", "LaunchAgents", `${AGENT_LABEL}.plist`);
      const plist = await readPlist(agentFile);
      expect(plist.Label).toBe(AGENT_LABEL);
      expect(plist.ProgramArguments).toEqual([`${installDir}/pohunek-web`]);
      expect(plist.EnvironmentVariables).toEqual({
        POHUNEK_BACKEND_STATIC_DIR: `${installDir}/frontend`,
        POHUNEK_BACKEND_LOG_DIR: join(harness.stateHome, "pohunek", "web-logs"),
        XDG_RUNTIME_DIR: runtime,
        POHUNEK_BACKEND_BIND_HOST: "100.64.0.7",
        POHUNEK_BACKEND_PORT: "8443",
        POHUNEK_BACKEND_DAEMON_SOCKET: hostile,
      });
      expect(plist.KeepAlive).toEqual({ SuccessfulExit: false });
      expect(plist.RunAtLoad).toBe(true);
      expect((await stat(agentFile)).mode & 0o777).toBe(0o644);
      expect((await stat(configFile)).mode & 0o777).toBe(0o600);
      expect((await stat(join(harness.stateHome, "pohunek", "web-logs"))).mode & 0o777).toBe(0o700);

      const launchctl = (await readFile(harness.launchctlLog, "utf8")).trim().split("\n");
      expect(launchctl).toEqual([
        `print gui/${FAKE_UID}/${AGENT_LABEL}`,
        `bootstrap gui/${FAKE_UID} ${agentFile}`,
      ]);
      // No other label is ever addressed: not the daemon, not the whole domain.
      expect(launchctl.join("\n")).not.toContain(".daemon");
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("keeps the agent unregistered until the listener is configured", async () => {
    const harness = await createHarness("macos");
    try {
      await createArchive(harness, "mac build", MACOS_TARGET);
      const result = await install(harness);
      expectSuccess(result);
      expect(result.stdout).toContain("then run ./install.sh again to register the agent");
      expect(await pathExists(harness.launchctlLog)).toBe(false);
      const agentFile = join(harness.home, "Library", "LaunchAgents", `${AGENT_LABEL}.plist`);
      const plist = await readPlist(agentFile);
      expect(Object.keys(plist.EnvironmentVariables as object).sort()).toEqual([
        "POHUNEK_BACKEND_LOG_DIR",
        "POHUNEK_BACKEND_STATIC_DIR",
      ]);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("an update restarts only the backend agent", async () => {
    const harness = await createHarness("macos");
    try {
      await createArchive(harness, "first", MACOS_TARGET);
      const configFile = join(harness.configHome, "pohunek", "backend.env");
      await mkdir(dirname(configFile), { recursive: true });
      await writeFile(configFile, "POHUNEK_BACKEND_BIND_HOST=100.64.0.7\nPOHUNEK_BACKEND_PORT=8443\n");
      expectSuccess(await install(harness));
      await writeFile(join(harness.archive, "frontend", "index.html"), "second");
      await sealManifest(harness.archive, MACOS_TARGET);
      expectSuccess(await install(harness));

      const agentFile = join(harness.home, "Library", "LaunchAgents", `${AGENT_LABEL}.plist`);
      const calls = (await readFile(harness.launchctlLog, "utf8")).trim().split("\n");
      expect(calls.slice(2)).toEqual([
        `print gui/${FAKE_UID}/${AGENT_LABEL}`,
        `bootout gui/${FAKE_UID}/${AGENT_LABEL}`,
        `print gui/${FAKE_UID}/${AGENT_LABEL}`,
        `bootstrap gui/${FAKE_UID} ${agentFile}`,
      ]);
      expect(
        await readFile(join(harness.dataHome, "pohunek", "web", "frontend", "index.html"), "utf8"),
      ).toBe("second");
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("refuses unsupported or duplicate settings before changing anything", async () => {
    for (const [content, reason] of [
      ["POHUNEK_BACKEND_BIND_HOST=100.64.0.7\nAWS_SECRET_ACCESS_KEY=abc\n", "not a supported backend setting"],
      ["POHUNEK_BACKEND_STATIC_DIR=/tmp/x\n", "managed by the installer"],
      ["POHUNEK_BACKEND_PORT=1\nPOHUNEK_BACKEND_PORT=2\n", "is set twice"],
      ["not a setting\n", "line without '='"],
    ] as const) {
      const harness = await createHarness("macos");
      try {
        await createArchive(harness, "mac build", MACOS_TARGET);
        const configFile = join(harness.configHome, "pohunek", "backend.env");
        await mkdir(dirname(configFile), { recursive: true });
        await writeFile(configFile, content);
        const result = await install(harness);
        expect(result.code).not.toBe(0);
        expect(result.stderr).toContain(reason);
        expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);
        expect(await pathExists(join(harness.home, "Library", "LaunchAgents", `${AGENT_LABEL}.plist`))).toBe(false);
        expect(await pathExists(harness.launchctlLog)).toBe(false);
      } finally {
        await rm(harness.root, { recursive: true, force: true });
      }
    }
  });

  test("requires an installed daemon and changes nothing without one", async () => {
    const harness = await createHarness("macos", { daemonInstalled: false });
    try {
      await createArchive(harness, "mac build", MACOS_TARGET);
      const result = await install(harness);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toContain("the daemon is not installed");
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);
      expect(await pathExists(join(harness.configHome, "pohunek"))).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("refuses a macOS older than the archive minimum", async () => {
    const harness = await createHarness("macos", { macosVersion: "13.6.1" });
    try {
      await createArchive(harness, "mac build", MACOS_TARGET);
      const result = await install(harness);
      expect(result.code).not.toBe(0);
      expect(result.stderr).toContain("needs macOS 14.0 or newer");
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });

  test("--uninstall removes the agent and files and keeps configuration and logs", async () => {
    const harness = await createHarness("macos");
    try {
      await createArchive(harness, "mac build", MACOS_TARGET);
      const configFile = join(harness.configHome, "pohunek", "backend.env");
      await mkdir(dirname(configFile), { recursive: true });
      await writeFile(configFile, "POHUNEK_BACKEND_BIND_HOST=100.64.0.7\nPOHUNEK_BACKEND_PORT=8443\n");
      expectSuccess(await install(harness));
      const logDir = join(harness.stateHome, "pohunek", "web-logs");
      await writeFile(join(logDir, "pohunek-backend.jsonl"), "{}\n");

      const result = await install(harness, ["--uninstall"]);
      expectSuccess(result);
      expect(await pathExists(join(harness.dataHome, "pohunek", "web"))).toBe(false);
      expect(await pathExists(join(harness.home, "Library", "LaunchAgents", `${AGENT_LABEL}.plist`))).toBe(false);
      expect(await pathExists(configFile)).toBe(true);
      expect(await pathExists(join(logDir, "pohunek-backend.jsonl"))).toBe(true);
      const calls = (await readFile(harness.launchctlLog, "utf8")).trim().split("\n");
      expect(calls).toContain(`bootout gui/${FAKE_UID}/${AGENT_LABEL}`);
      expect(calls.join("\n")).not.toContain(".daemon");
    } finally {
      await rm(harness.root, { recursive: true, force: true });
    }
  });
});

async function createHarness(
  host: "linux" | "macos",
  options: { readonly daemonInstalled?: boolean; readonly macosVersion?: string } = {},
): Promise<Harness> {
  const root = await realpathTemp();
  const commands = join(root, "commands");
  await mkdir(commands, { recursive: true });
  const system = host === "macos" ? "Darwin" : "Linux";
  const machine = host === "macos" ? "arm64" : "x86_64";
  await writeShim(
    commands,
    "uname",
    `#!/bin/sh\ncase "$1" in -s) echo ${system} ;; -m) echo ${machine} ;; *) exit 2 ;; esac\n`,
  );
  await writeShim(commands, "id", `#!/bin/sh\nif [ "$1" = "-u" ]; then echo ${FAKE_UID}; else exec /usr/bin/id "$@"; fi\n`);
  const launchctlLog = join(root, "launchctl.log");
  const loadedMarker = join(root, "agent.loaded");
  if (host === "macos") {
    await writeShim(commands, "sw_vers", `#!/bin/sh\necho ${options.macosVersion ?? "15.1"}\n`);
    await writeShim(commands, "plutil", FAKE_PLUTIL);
    await writeShim(commands, "launchctl", FAKE_LAUNCHCTL);
    const installed = options.daemonInstalled ?? true;
    const status = installed
      ? `{\\n  "ok": {\\n    "installed": true,\\n    "namespace": "${NAMESPACE}",\\n    "prefix": "/x"\\n  }\\n}`
      : `{\\n  "ok": {\\n    "installed": false,\\n    "namespace": null\\n  }\\n}`;
    const pohunek = join(root, "home", ".local", "bin", "pohunek");
    await mkdir(dirname(pohunek), { recursive: true });
    await writeFile(pohunek, `#!/bin/sh\nprintf '${status}\\n'\n`, { mode: 0o755 });
  }
  const home = join(root, "home");
  await mkdir(home, { recursive: true });
  const harness: Harness = {
    root,
    archive: join(root, "archive dir"),
    home,
    dataHome: join(root, "data home"),
    configHome: join(root, "config home"),
    stateHome: join(root, "state home"),
    commands,
    launchctlLog,
    loadedMarker,
    environment: {
      ...process.env,
      PATH: `${commands}:${process.env.PATH ?? "/usr/bin:/bin"}`,
      HOME: home,
      XDG_CONFIG_HOME: join(root, "config home"),
      XDG_DATA_HOME: join(root, "data home"),
      XDG_STATE_HOME: join(root, "state home"),
      FAKE_LAUNCHCTL_LOG: launchctlLog,
      FAKE_LAUNCHCTL_LOADED: loadedMarker,
    },
  };
  delete harness.environment.POHUNEK_BIN;
  delete harness.environment.XDG_RUNTIME_DIR;
  return harness;
}

async function realpathTemp(): Promise<string> {
  const { realpath } = await import("node:fs/promises");
  return realpath(await mkdtemp(join(tmpdir(), "pohunek-web-install-test-")));
}

async function writeShim(directory: string, name: string, text: string): Promise<void> {
  const path = join(directory, name);
  await writeFile(path, text);
  await chmod(path, 0o755);
}

async function createArchive(
  harness: Harness,
  indexContent: string,
  target: string,
  component = "web",
): Promise<void> {
  const path = harness.archive;
  await rm(path, { recursive: true, force: true });
  await mkdir(join(path, "frontend"), { recursive: true });
  await mkdir(join(path, "packaging"), { recursive: true });
  await writeFile(join(path, "pohunek-web"), "#!/usr/bin/env sh\nexit 0\n", { mode: 0o755 });
  await writeFile(join(path, "frontend", "index.html"), indexContent);
  if (target === LINUX_TARGET) {
    await cp(SERVICE_TEMPLATE, join(path, "pohunek-backend.service.in"));
  }
  await cp(join(RELEASE_DIR, "backend.env.example"), join(path, "backend.env.example"));
  await cp(join(RELEASE_DIR, "install.sh"), join(path, "install.sh"));
  await cp(VERIFY_ARCHIVE, join(path, "packaging", "verify-archive"));
  // The installer refuses a tree another account could write, whatever the
  // umask of the machine running the test.
  await execFileAsync("chmod", ["-R", "go-w", path]);
  await sealManifest(path, target, component);
}

async function sealManifest(path: string, target: string, component = "web"): Promise<void> {
  const args = [WRITE_MANIFEST, path, component, "1.2.3", target, "none"];
  if (target === MACOS_TARGET) {
    args.push(MINIMUM_MACOS);
  }
  await execFileAsync("sh", args);
}

async function install(
  harness: Harness,
  args: readonly string[] = [],
  extraEnvironment: NodeJS.ProcessEnv = {},
): Promise<RunResult> {
  try {
    const { stdout, stderr } = await execFileAsync("sh", [join(harness.archive, "install.sh"), ...args], {
      env: { ...harness.environment, ...extraEnvironment },
    });
    return { code: 0, stdout, stderr };
  } catch (error: unknown) {
    const failure = error as { readonly code?: number; readonly stdout?: string; readonly stderr?: string };
    return { code: failure.code ?? 1, stdout: failure.stdout ?? "", stderr: failure.stderr ?? "" };
  }
}

function expectSuccess(result: RunResult): void {
  expect(result.stderr).toBe("");
  expect(result.code).toBe(0);
}

async function readPlist(path: string): Promise<Record<string, unknown>> {
  const { stdout } = await execFileAsync("python3", [
    "-c",
    "import json, plistlib, sys; print(json.dumps(plistlib.load(open(sys.argv[1], 'rb'))))",
    path,
  ]);
  return JSON.parse(stdout) as Record<string, unknown>;
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
