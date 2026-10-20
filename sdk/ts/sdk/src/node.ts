import { attachRawTransport, connectRawTransport, type RawStream } from "./attach";
import { Client } from "./client";
import type { ConnectOptions } from "./transport";
import { SocketTransport } from "./transport-socket";

const LOCAL_HOST = "local";

/** A host route resolved once: this machine, or one pinned overlay peer. */
export type ResolvedHost =
  | { target: "local" }
  | {
      target: "remote";
      /** The route as the caller typed it, for errors and output. */
      display: string;
      /** The address and effective daemon port the peer is dialed at. */
      address: { host: string; port: number };
      /**
       * Names the peer by its stable identity. Resolving it again selects the
       * same peer after a name or address change.
       */
      pinnedRoute: string;
      overlay: string;
      peerId?: string;
      fqdn?: string;
    };

function isLocalHost(host: string): boolean {
  return host.length === 0 || host === LOCAL_HOST;
}

/**
 * Resolves a host route as `--host` accepts it, through the local daemon.
 *
 * `local` and the empty route answer without contacting the daemon. Any other
 * route is resolved by the daemon on `socketPath` with the rules of the CLI and
 * the Rust client: this machine's own overlay name is `local`, a name that this
 * machine and a peer share is `overlay_host_ambiguous`, and a peer is returned
 * with a `pinnedRoute` that keeps selecting it. Fails with
 * `daemon_protocol_too_old` against a daemon of the previous protocol version.
 */
export async function resolveHost(
  route: string,
  socketPath: string,
  opts?: ConnectOptions,
): Promise<ResolvedHost> {
  if (isLocalHost(route)) {
    return { target: "local" };
  }
  const client = await connectLocal(socketPath, opts);
  try {
    const resolution = await client.call("host.resolve", { route });
    if (resolution.target === "local") {
      return { target: "local" };
    }
    return {
      target: "remote",
      display: route,
      address: { host: resolution.address, port: resolution.port },
      pinnedRoute: resolution.pinned_route,
      overlay: resolution.overlay,
      ...(resolution.peer_id === undefined ? {} : { peerId: resolution.peer_id }),
      ...(resolution.fqdn === undefined ? {} : { fqdn: resolution.fqdn }),
    };
  } finally {
    await client.close();
  }
}

/**
 * Connects a client to the daemon a host route names.
 *
 * The route is resolved once; the returned client keeps the resolved endpoint
 * for every connection it opens.
 */
export async function connectHost(
  route: string,
  socketPath: string,
  opts?: ConnectOptions,
): Promise<Client> {
  const resolved = await resolveHost(route, socketPath, opts);
  if (resolved.target === "local") {
    return connectLocal(socketPath, opts);
  }
  return connectTcp(resolved.display, resolved.address, opts);
}

/** Connects a client to a daemon over its local Unix socket. */
export async function connectLocal(socketPath: string, opts?: ConnectOptions): Promise<Client> {
  return Client.connectTransport(SocketTransport.unix(socketPath, opts), opts);
}

/** Connects a client to a daemon at an explicit TCP address. */
export async function connectTcp(
  host: string,
  addr: { host: string; port: number },
  opts?: ConnectOptions,
): Promise<Client> {
  return Client.connectTransport(SocketTransport.tcp(host, addr, opts), opts, host);
}

/** Opens an unframed stream over a local Unix socket. */
export async function connectRawLocal(socketPath: string, opts?: ConnectOptions): Promise<RawStream> {
  return connectRawTransport(SocketTransport.unix(socketPath, opts));
}

/** Opens an unframed stream at an explicit TCP address. */
export async function connectRawTcp(
  host: string,
  addr: { host: string; port: number },
  opts?: ConnectOptions,
): Promise<RawStream> {
  return connectRawTransport(SocketTransport.tcp(host, addr, opts));
}

/**
 * Redeems an attach stream on the daemon a host route names.
 *
 * A remote route is resolved once through the local daemon on `socketPath`.
 */
export async function attachRaw(
  host: string,
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  const resolved = await resolveHost(host, socketPath, opts);
  if (resolved.target === "local") {
    return attachRawLocal(socketPath, streamId, opts);
  }
  return attachRawTcp(resolved.display, resolved.address, streamId, opts);
}

/** Redeems an attach stream over a local Unix socket. */
export async function attachRawLocal(
  socketPath: string,
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  return attachRawTransport(SocketTransport.unix(socketPath, opts), streamId);
}

/** Redeems an attach stream at an explicit TCP address. */
export async function attachRawTcp(
  host: string,
  addr: { host: string; port: number },
  streamId: string,
  opts?: ConnectOptions,
): Promise<RawStream> {
  return attachRawTransport(SocketTransport.tcp(host, addr, opts), streamId, host);
}

export { SocketTransport };

export {
  APP_DIR,
  ENV_XDG_RUNTIME_DIR,
  MACOS_DEFAULT_RUNTIME_PREFIX,
  RuntimePathError,
  SOCKET_NAME,
  SOCKET_PATH_MAX_BYTES,
  currentRuntimePathContext,
  resolveDaemonSocket,
  resolveRuntimeDir,
  validateSocketPath,
  verifyDaemonRuntime,
} from "./runtime-paths";
export type {
  RuntimePathContext,
  RuntimePathFailure,
  RuntimePathReason,
  RuntimePlatform,
} from "./runtime-paths";
