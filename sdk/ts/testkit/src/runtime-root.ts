import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Parent of fixture roots on macOS. `os.tmpdir()` is a `/var/folders/...`
 * path: it sits below the `/var` symlink, which the daemon's trusted-directory
 * checks refuse, and it is long enough that nested runtime sockets exceed
 * Darwin's 103-byte `sun_path` limit.
 */
export const MACOS_FIXTURE_ROOT_PARENT = "/private/tmp";

/**
 * Longest daemon runtime path below a fixture root that must still fit a
 * socket: `<root>/runtime/pohunek/daemon.sock`.
 */
const DAEMON_SOCKET_SUFFIX = "/runtime/pohunek/daemon.sock";

/** Darwin's `sockaddr_un.sun_path` capacity, the stricter of the supported platforms. */
const SOCKET_PATH_BUDGET_BYTES = 103;

export interface FixtureRootOptions {
  /** Directory the root is created in; defaults to the platform's fixture parent. */
  readonly parent?: string;
}

/**
 * Creates a private (mode 0700), canonical directory for an isolated real-daemon
 * run. Pass a short `prefix`: it counts against the socket path limit.
 *
 * Throws when the daemon socket below the root would not fit a Unix socket
 * path, so a long `TMPDIR` fails here with the cause instead of inside the daemon.
 */
export function createFixtureRoot(
  prefix: string,
  options: FixtureRootOptions = {},
): Promise<string> {
  try {
    return Promise.resolve(createFixtureRootSync(prefix, options));
  } catch (error: unknown) {
    return Promise.reject(error instanceof Error ? error : new Error(String(error)));
  }
}

/** Synchronous form of {@link createFixtureRoot}, for module-level fixtures. */
export function createFixtureRootSync(prefix: string, options: FixtureRootOptions = {}): string {
  const parent = options.parent
    ?? (process.platform === "darwin" ? MACOS_FIXTURE_ROOT_PARENT : tmpdir());
  const root = realpathSync(mkdtempSync(join(parent, prefix)));
  const socketBytes = Buffer.byteLength(`${root}${DAEMON_SOCKET_SUFFIX}`);
  if (socketBytes > SOCKET_PATH_BUDGET_BYTES) {
    rmSync(root, { recursive: true, force: true });
    throw new Error(
      `fixture root ${root} leaves a ${socketBytes}-byte daemon socket path, over the `
        + `${SOCKET_PATH_BUDGET_BYTES}-byte limit; use a shorter TMPDIR or prefix`,
    );
  }
  return root;
}
