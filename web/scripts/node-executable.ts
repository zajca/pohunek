import { constants } from "node:fs";
import { access, stat } from "node:fs/promises";
import { delimiter, isAbsolute, join } from "node:path";

export const NODE_EXECUTABLE_ENV = "POHUNEK_NODE_BIN";

const NODE_PROGRAM = "node";

/**
 * Install prefixes searched after `PATH` on macOS. An app or terminal started
 * outside a login shell carries a minimal `PATH` that omits the Homebrew prefix
 * (Apple Silicon) and the package-installer prefix (Intel and the nodejs.org
 * installer).
 */
export const MACOS_NODE_DIRECTORIES: readonly string[] = ["/opt/homebrew/bin", "/usr/local/bin"];

export class NodeExecutableError extends Error {
  public override readonly name = "NodeExecutableError";
}

export interface NodeDiscoveryOptions {
  readonly env: NodeJS.ProcessEnv;
  readonly platform: NodeJS.Platform;
  /** Directories searched after `PATH`; defaults to the platform's install prefixes. */
  readonly fallbackDirectories?: readonly string[];
}

/**
 * Finds the Node executable the Vite dev server runs under.
 *
 * Vite's WebSocket proxy needs Node `net.Socket` APIs Bun does not implement,
 * so the dev stack starts Node as a child. `POHUNEK_NODE_BIN` is authoritative
 * and must be an absolute path to an executable file; without it the first
 * executable `node` on `PATH` wins, then the platform's install prefixes.
 */
export async function resolveNodeExecutable(options: NodeDiscoveryOptions): Promise<string> {
  const configured = options.env[NODE_EXECUTABLE_ENV];
  if (configured !== undefined) {
    if (!isAbsolute(configured)) {
      throw new NodeExecutableError(`${NODE_EXECUTABLE_ENV} must be an absolute path`);
    }
    if (!(await isExecutableFile(configured))) {
      throw new NodeExecutableError(`${NODE_EXECUTABLE_ENV} is not an executable file: ${configured}`);
    }
    return configured;
  }

  const fallback = options.fallbackDirectories
    ?? (options.platform === "darwin" ? MACOS_NODE_DIRECTORIES : []);
  const searched = [...absoluteDirectories(options.env["PATH"]), ...fallback];
  for (const directory of searched) {
    const candidate = join(directory, NODE_PROGRAM);
    if (await isExecutableFile(candidate)) {
      return candidate;
    }
  }
  throw new NodeExecutableError(
    `Node was not found on PATH or in ${fallback.length === 0 ? "any fallback directory" : fallback.join(", ")}; `
      + `install Node or set ${NODE_EXECUTABLE_ENV} to its absolute path`,
  );
}

function absoluteDirectories(path: string | undefined): string[] {
  if (path === undefined) {
    return [];
  }
  return path.split(delimiter).filter((entry) => isAbsolute(entry));
}

async function isExecutableFile(path: string): Promise<boolean> {
  try {
    if (!(await stat(path)).isFile()) {
      return false;
    }
    await access(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}
