import { BackendConfigError, ENV_XDG_RUNTIME_DIR, loadBackendConfig, type BackendConfig } from "./config";
import { RuntimePathError, verifyDaemonRuntime } from "./runtime-paths";
import { BackendStartupError, startHostsPipeline, type HostsPipelineHandle } from "./hosts";
import { errorClass, stdoutLogger, type BackendLogEvent, type BackendLogger } from "./log";
import { LogFileError, rotatingFileLogger } from "./log-files";
import { startBackendServer, type BackendServerHandle } from "./server";

export interface BackendHandle {
  readonly url: string;
  readonly port: number;
  readonly hosts: HostsPipelineHandle;
  close(): Promise<void>;
}

export async function startBackend(
  config: BackendConfig,
  logger: BackendLogger = stdoutLogger,
): Promise<BackendHandle> {
  verifyDerivedRuntime(config);
  const hosts = await startHostsPipeline({
    daemonSocketPath: config.daemonSocketPath,
    discoverIntervalSeconds: config.discoverIntervalSeconds,
    logger,
  });

  let server: BackendServerHandle;
  try {
    server = await startBackendServer({
      bindHost: config.bindHost,
      port: config.port,
      allowLoopbackBind: config.allowLoopbackBind,
      staticAssetsDir: config.staticAssetsDir,
      hosts,
      logger,
    });
  } catch (error: unknown) {
    await hosts.close();
    throw error;
  }

  try {
    logger.log({
      level: "info",
      event: "backend_server",
      lifecycle: "listening",
      status: "ok",
      port: server.port,
      url: server.url,
    });
  } catch (error: unknown) {
    await server.close();
    await hosts.close();
    throw error;
  }

  return {
    url: server.url,
    port: server.port,
    hosts,
    close: async (): Promise<void> => {
      await server.close();
      await hosts.close();
      logger.log({
        level: "info",
        event: "backend_server",
        lifecycle: "closed",
        status: "ok",
      });
    },
  };
}

/**
 * Checks the runtime directory a derived socket lives in before any connection
 * is made, so a socket planted in a shared directory is never dialed.
 */
function verifyDerivedRuntime(config: BackendConfig): void {
  const runtime = config.derivedRuntime;
  if (runtime === undefined) {
    return;
  }
  try {
    verifyDaemonRuntime(runtime.dir, config.daemonSocketPath, runtime.effectiveUid, ENV_XDG_RUNTIME_DIR);
  } catch (error: unknown) {
    if (error instanceof RuntimePathError) {
      throw new BackendConfigError(error.variable, error.message);
    }
    throw error;
  }
}

/**
 * Starts the backend from its environment. Without an explicit logger the
 * destination follows the configuration: a rotating owner-private file family
 * when `POHUNEK_BACKEND_LOG_DIR` is set (launchd keeps no journal), else
 * standard output. A file logger created here is closed with the backend; a
 * failed start is recorded in it before it is closed, since launchd discards
 * standard output.
 */
export async function startBackendFromEnv(
  env: NodeJS.ProcessEnv = process.env,
  logger?: BackendLogger,
): Promise<BackendHandle> {
  const config = loadBackendConfig(env);
  const owned = logger === undefined && config.logFiles !== undefined
    ? rotatingFileLogger(config.logFiles)
    : undefined;
  try {
    const handle = await startBackend(config, logger ?? owned ?? stdoutLogger);
    if (owned === undefined) {
      return handle;
    }
    return {
      ...handle,
      close: async (): Promise<void> => {
        try {
          await handle.close();
        } finally {
          owned.close();
        }
      },
    };
  } catch (error: unknown) {
    if (owned !== undefined) {
      owned.log(startupFailedEvent(error));
      owned.close();
    }
    throw error;
  }
}

function startupFailedEvent(error: unknown): BackendLogEvent {
  return {
    level: "error",
    event: "backend_startup",
    lifecycle: "failed",
    status: "failed",
    error_class: errorClass(error),
  };
}

export function runBackend(): void {
  void Promise.resolve()
    .then((): Promise<BackendHandle> => startBackendFromEnv())
    .catch((error: unknown): void => {
      stdoutLogger.log(startupFailedEvent(error));
      console.error(
        error instanceof BackendStartupError || error instanceof BackendConfigError || error instanceof LogFileError
          ? error.message
          : `Cannot start @pohunek/backend (${errorClass(error)}). Check the backend configuration.`,
      );
      process.exitCode = 1;
    });
}
