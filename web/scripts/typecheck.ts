// Run every web workspace typecheck task in parallel.
//
// The root `typecheck` script used to chain seven `tsc` / `svelte-check`
// invocations with `&&`, so the wall time summed every package even though
// the tasks are independent. This orchestrator keeps each package script as
// the single source of truth and runs the tasks concurrently: the wall time
// lands near the slowest task instead of their sum, and every failing task
// is reported instead of stopping at the first one.

import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
// Spawn Bun by its own executable path so the orchestrator does not depend on
// what `bun` resolves to on `PATH`.
const BUN_EXECUTABLE = process.execPath;
// Hoisted workspace TypeScript binary. The root tsc projects must not go
// through `bun run typecheck`, which would recurse into this orchestrator.
const TSC_EXECUTABLE = join(WEB_ROOT, "node_modules", "typescript", "bin", "tsc");
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

const PACKAGES = ["shared", "sdk", "backend", "testkit", "client-core", "frontend"] as const;

const TASKS: readonly TypecheckTask[] = [
  {
    name: "root",
    cwd: WEB_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "tsconfig.json"],
  },
  {
    name: "release-test",
    cwd: WEB_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "release/test/tsconfig.json"],
  },
  ...PACKAGES.map(
    (name): TypecheckTask => ({
      name,
      cwd: join(WEB_ROOT, name),
      args: ["run", "typecheck"],
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
  const results = await Promise.all(TASKS.map(runTask));
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
