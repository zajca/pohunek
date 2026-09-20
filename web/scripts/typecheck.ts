// Run the composite TypeScript build graph plus the standalone checks in one
// command.
//
// `tsc -b web/tsconfig.json` typechecks the shared/sdk/backend/testkit/
// client-core/tools graph incrementally: unchanged referenced projects are
// reported "up to date" from their `.tsbuildinfo` instead of being rechecked.
// The frontend (`svelte-check`), its Playwright e2e project, and the release
// installer fixture cannot join the composite graph (Svelte files are not
// valid composite inputs, the e2e project imports SDK sources outside its
// `rootDir`, and the release fixture needs `bun-types`), so this orchestrator
// runs `tsc -b` first and then those three standalone checks concurrently.
// Every failing task is reported instead of stopping at the first one.

import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
// Spawn Bun by its own executable path so the orchestrator does not depend on
// what `bun` resolves to on `PATH`.
const BUN_EXECUTABLE = process.execPath;
// Hoisted workspace TypeScript binary. The composite build must not go
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

const TASKS: readonly TypecheckTask[] = [
  {
    name: "build",
    cwd: WEB_ROOT,
    args: [TSC_EXECUTABLE, "-b", "tsconfig.json"],
  },
  {
    name: "release-test",
    cwd: WEB_ROOT,
    args: [TSC_EXECUTABLE, "--noEmit", "-p", "release/test/tsconfig.json"],
  },
  {
    name: "frontend",
    cwd: join(WEB_ROOT, "frontend"),
    args: ["run", "typecheck"],
  },
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
  // The standalone checks import workspace sources, so they must observe the
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
