// Run the composite TypeScript build graph plus the standalone checks in one
// command.
//
// `tsc -b tsconfig.json` (repository root) typechecks the protocol/sdk/testkit
// packages under `sdk/ts/` and the backend/client-core/tools projects under
// `web/` as one source graph incrementally: unchanged referenced projects
// are reported "up to date" from their `.tsbuildinfo` instead of being
// rechecked.
//
// Package tests stay out of that composite graph on purpose. Test files import
// sibling packages (`@pohunek/backend`, `@pohunek/testkit`, ...) whose exports
// resolve to `.ts` sources, so a composite project that includes `test/**/*`
// silently compiles sources of non-referenced projects, and the resulting test
// dependencies form cycles that project references cannot express. Each
// package therefore carries a standalone `<pkg>/test/tsconfig.json`
// (non-composite, noEmit) that this orchestrator runs after `tsc -b` finished,
// in parallel with the frontend (`svelte-check`), its Playwright e2e project,
// and the release installer fixture. Every failing task is reported instead of
// stopping at the first one.

import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
// Spawn Bun by its own executable path so the orchestrator does not depend on
// what `bun` resolves to on `PATH`.
const BUN_EXECUTABLE = process.execPath;
// Hoisted workspace TypeScript binary. The composite build must not go
// through `bun run typecheck`, which would recurse into this orchestrator.
const TSC_EXECUTABLE = join(REPO_ROOT, "node_modules", "typescript", "bin", "tsc");
const MS_PER_SECOND = 1000;

interface TypecheckTask {
  readonly name: string;
  readonly cwd: string;
  readonly args: string[];
}

interface TypecheckResult {
  readonly name: string;
  readonly ok: boolean;
  readonly durationMs: number;
  readonly output: string;
}

// Workspace directories (relative to the repository root) that carry a
// standalone `test/tsconfig.json`.
const TEST_PROJECTS = [
  "sdk/ts/sdk",
  "sdk/ts/testkit",
  "web/backend",
  "web/client-core",
] as const;

const TASKS: readonly TypecheckTask[] = [
  {
    name: "build",
    cwd: REPO_ROOT,
    args: [TSC_EXECUTABLE, "-b", "tsconfig.json"],
  },
  {
    name: "release-test",
    cwd: REPO_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "web/release/test/tsconfig.json"],
  },
  {
    name: "sdk-release",
    cwd: REPO_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "sdk/ts/scripts/tsconfig.json"],
  },
  {
    name: "scripts-test",
    cwd: REPO_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "web/scripts/test/tsconfig.json"],
  },
  {
    name: "frontend",
    cwd: join(REPO_ROOT, "web", "frontend"),
    args: ["run", "typecheck"],
  },
  ...TEST_PROJECTS.map(
    (name): TypecheckTask => ({
      name: `${name.replace(/^(?:sdk\/ts|web)\//u, "")}-test`,
      cwd: join(REPO_ROOT, name),
      args: [TSC_EXECUTABLE, "--noEmit", "-p", "test/tsconfig.json"],
    }),
  ),
];

function runTask(task: TypecheckTask): Promise<TypecheckResult> {
  return new Promise((resolve) => {
    const startedAt = performance.now();
    const child = spawn(BUN_EXECUTABLE, task.args, {
      cwd: task.cwd,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    const collect = (chunk: unknown): void => {
      output += String(chunk);
    };
    child.stdout?.on("data", collect);
    child.stderr?.on("data", collect);
    child.on("error", (error: Error): void => {
      resolve({
        name: task.name,
        ok: false,
        durationMs: performance.now() - startedAt,
        output: `${output}\n${error.message}`,
      });
    });
    child.on("close", (code: number | null): void => {
      resolve({
        name: task.name,
        ok: code === 0,
        durationMs: performance.now() - startedAt,
        output,
      });
    });
  });
}

async function main(): Promise<void> {
  // The standalone checks typecheck workspace sources, so they must observe the
  // composite build's output state, not race it: run `tsc -b` to completion
  // first, then fan out the remaining independent checks.
  const build = await runTask(TASKS[0] as TypecheckTask);
  const standalone = await Promise.all(TASKS.slice(1).map(runTask));
  const results = [build, ...standalone];
  const failed = results.filter((result) => !result.ok);
  for (const result of results) {
    const seconds = (result.durationMs / MS_PER_SECOND).toFixed(1);
    if (result.ok) {
      console.log(`[typecheck] ${result.name} (${seconds}s): ok`);
      continue;
    }
    console.error(`[typecheck] ${result.name} (${seconds}s): FAILED`);
    for (const line of result.output.split("\n")) {
      if (line.length > 0) {
        console.error(`[typecheck][${result.name}] ${line}`);
      }
    }
  }
  if (failed.length > 0) {
    const names = failed.map((result) => result.name).join(", ");
    console.error(`[typecheck] ${failed.length}/${results.length} tasks failed: ${names}`);
    process.exitCode = 1;
    return;
  }
  console.log(`[typecheck] all ${results.length} tasks passed`);
}

await main();
