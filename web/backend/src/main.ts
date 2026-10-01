import { loadBackendConfig, type BackendConfig } from "./config";
import { BackendStartupError, startHostsPipeline, type HostsPipelineHandle } from "./hosts";
import { errorClass, stdoutLogger, type BackendLogger } from "./log";
import { rotatingFileLogger } from "./log-files";
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

  logger.log({
    level: "info",
    event: "backend_server",
    lifecycle: "listening",
    status: "ok",
    port: server.port,
    url: server.url,
  });

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
 * Starts the backend from its environment. The logger defaults to the
 * configured destination: a rotating owner-private file family when
 * `POHUNEK_BACKEND_LOG_DIR` is set (launchd keeps no journal), else standard output.
 */
export function startBackendFromEnv(
  env: NodeJS.ProcessEnv = process.env,
  logger?: BackendLogger,
): Promise<BackendHandle> {
  const config = loadBackendConfig(env);
  return startBackend(config, logger ?? loggerFor(config));
}

function loggerFor(config: BackendConfig): BackendLogger {
  return config.logFiles === undefined ? stdoutLogger : rotatingFileLogger(config.logFiles);
}

export function runBackend(): void {
  void Promise.resolve()
    .then((): Promise<BackendHandle> => startBackendFromEnv())
    .catch((error: unknown): void => {
      stdoutLogger.log({
        level: "error",
        event: "backend_startup",
        lifecycle: "failed",
        status: "failed",
        error_class: errorClass(error),
      });
      console.error(
        error instanceof BackendStartupError
          ? error.message
          : `Cannot start @pohunek/backend (${errorClass(error)}). Check the backend configuration.`,
      );
      process.exitCode = 1;
    });
}
