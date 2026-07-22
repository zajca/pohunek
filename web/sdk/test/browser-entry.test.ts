import { existsSync, readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "bun:test";
import * as browserSdk from "@pohunek/sdk/browser";

const SDK_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const BROWSER_ENTRY = resolve(SDK_ROOT, "src/index.browser.ts");
const PROTOCOL_ENTRY = resolve(SDK_ROOT, "../shared/src/index.ts");
const MODULE_SPECIFIER_PATTERN = /(?:from\s*|import\s*\(\s*|import\s*)["']([^"']+)["']/gu;

describe("browser entry", () => {
  test("package subpath exposes browser transports without socket APIs", () => {
    expect(typeof browserSdk.Client).toBe("function");
    expect(typeof browserSdk.WsTransport).toBe("function");
    expect("SocketTransport" in browserSdk).toBe(false);
    expect("connectLocal" in browserSdk).toBe(false);
  });

  test("loads and creates request ids without crypto.randomUUID", () => {
    const script = [
      'Object.defineProperty(globalThis.crypto, "randomUUID", { configurable: true, value: undefined });',
      'const { nextRequestId } = await import("@pohunek/sdk/browser");',
      'process.stdout.write(`${nextRequestId("browser")}\\n${nextRequestId("browser")}`);',
    ].join("\n");
    const child = spawnSync(process.execPath, ["--eval", script], {
      cwd: SDK_ROOT,
      env: process.env,
      encoding: "utf8",
    });
    if (child.status !== 0) {
      throw new Error(`isolated browser import failed: ${child.stderr}`);
    }

    const [first, second] = child.stdout.split("\n");
    const firstMatch = /^sdk-browser-([0-9a-f]+)-0$/u.exec(first ?? "");
    expect(firstMatch === null).toBe(false);
    const runToken = firstMatch?.[1] ?? "";
    expect(runToken.length).toBeGreaterThan(31);
    expect(second).toBe(`sdk-browser-${runToken}-1`);
  });

  test("transitive module graph has no node: specifiers", () => {
    const visited = new Set<string>();
    const pending = [BROWSER_ENTRY];
    const nodeImports: string[] = [];

    while (pending.length > 0) {
      const modulePath = pending.pop();
      if (modulePath === undefined || visited.has(modulePath)) {
        continue;
      }
      visited.add(modulePath);

      const source = readFileSync(modulePath, "utf8");
      for (const match of source.matchAll(MODULE_SPECIFIER_PATTERN)) {
        const specifier = match[1];
        if (specifier === undefined) {
          continue;
        }
        if (specifier.startsWith("node:")) {
          nodeImports.push(`${modulePath}: ${specifier}`);
          continue;
        }

        pending.push(resolveGraphDependency(modulePath, specifier));
      }
    }

    expect(nodeImports).toEqual([]);
  });
});

function resolveGraphDependency(modulePath: string, specifier: string): string {
  if (specifier === "@pohunek/protocol") {
    return PROTOCOL_ENTRY;
  }
  if (!specifier.startsWith(".")) {
    throw new Error(`browser graph contains an uninspected package import '${specifier}' in '${modulePath}'`);
  }

  const candidate = resolve(dirname(modulePath), specifier);
  for (const path of [`${candidate}.ts`, resolve(candidate, "index.ts")]) {
    if (existsSync(path)) {
      return path;
    }
  }
  throw new Error(`cannot resolve browser graph dependency '${specifier}' from '${modulePath}'`);
}
