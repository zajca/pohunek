import { readFile, stat } from "node:fs/promises";
import { extname, resolve, sep } from "node:path";
import type { HostsPipelineHandle } from "./hosts";
import { errorClass, stdoutLogger, type BackendLogger } from "./log";
import { startRelay, type RelayHandle } from "./relay";

const API_HOSTS_PATH = "/api/hosts";
const INDEX_FILENAME = "index.html";
const METHOD_GET = "GET";
const METHOD_HEAD = "HEAD";
const HEADER_CONTENT_TYPE = "content-type";
const HEADER_CACHE_CONTROL = "cache-control";
const CONTENT_TYPE_JSON = "application/json; charset=utf-8";
const CONTENT_TYPE_TEXT = "text/plain; charset=utf-8";
const CONTENT_TYPES = new Map<string, string>([
  [".css", "text/css; charset=utf-8"],
  [".gif", "image/gif"],
  [".html", "text/html; charset=utf-8"],
  [".ico", "image/x-icon"],
  [".jpeg", "image/jpeg"],
  [".jpg", "image/jpeg"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", CONTENT_TYPE_JSON],
  [".map", "application/json; charset=utf-8"],
  [".png", "image/png"],
  [".svg", "image/svg+xml"],
  [".txt", CONTENT_TYPE_TEXT],
  [".wasm", "application/wasm"],
  [".webmanifest", "application/manifest+json"],
  [".woff", "font/woff"],
  [".woff2", "font/woff2"],
]);

export interface StartBackendServerOptions {
  readonly bindHost: string;
  readonly port: number;
  readonly allowLoopbackBind: boolean;
  readonly staticAssetsDir: string;
  readonly hosts: HostsPipelineHandle;
  readonly logger?: BackendLogger;
}

export type BackendServerHandle = RelayHandle;

export function startBackendServer(
  options: StartBackendServerOptions,
): Promise<BackendServerHandle> {
  const logger = options.logger ?? stdoutLogger;
  const staticAssetsRoot = resolve(options.staticAssetsDir);
  return startRelay({
    bindHost: options.bindHost,
    port: options.port,
    targets: (host: string) => options.hosts.targetForHost(host),
    allowLoopbackBind: options.allowLoopbackBind,
    httpHandler: (request: Request) => handleHttpRequest(request, options.hosts, staticAssetsRoot, logger),
  });
}

async function handleHttpRequest(
  request: Request,
  hosts: HostsPipelineHandle,
  staticAssetsRoot: string,
  logger: BackendLogger,
): Promise<Response> {
  const startedAt = performance.now();
  const url = new URL(request.url);
  let response: Response;
  try {
    response = url.pathname === API_HOSTS_PATH
      ? handleHostsRequest(request, hosts)
      : await serveStatic(request, url.pathname, staticAssetsRoot);
  } catch (error: unknown) {
    logger.log({
      level: "error",
      event: "http_request",
      method: request.method,
      duration_ms: elapsedMilliseconds(startedAt),
      status: 500,
      error_class: errorClass(error),
    });
    return new Response("internal server error", {
      status: 500,
      headers: { [HEADER_CONTENT_TYPE]: CONTENT_TYPE_TEXT },
    });
  }

  logger.log({
    level: "info",
    event: "http_request",
    method: request.method,
    duration_ms: elapsedMilliseconds(startedAt),
    status: response.status,
  });
  return response;
}

function handleHostsRequest(request: Request, hosts: HostsPipelineHandle): Response {
  if (request.method !== METHOD_GET && request.method !== METHOD_HEAD) {
    return new Response("method not allowed", {
      status: 405,
      headers: {
        allow: `${METHOD_GET}, ${METHOD_HEAD}`,
        [HEADER_CONTENT_TYPE]: CONTENT_TYPE_TEXT,
      },
    });
  }
  const body = request.method === METHOD_HEAD ? null : JSON.stringify(hosts.snapshot());
  return new Response(body, {
    status: 200,
    headers: {
      [HEADER_CONTENT_TYPE]: CONTENT_TYPE_JSON,
      [HEADER_CACHE_CONTROL]: "no-store",
    },
  });
}

async function serveStatic(request: Request, pathname: string, root: string): Promise<Response> {
  if (request.method !== METHOD_GET && request.method !== METHOD_HEAD) {
    return new Response("method not allowed", {
      status: 405,
      headers: {
        allow: `${METHOD_GET}, ${METHOD_HEAD}`,
        [HEADER_CONTENT_TYPE]: CONTENT_TYPE_TEXT,
      },
    });
  }

  const relativePath = safeRelativePath(pathname);
  if (relativePath === undefined) {
    return new Response("not found", { status: 404 });
  }

  const requestedPath = resolve(root, relativePath.length === 0 ? INDEX_FILENAME : relativePath);
  const existingFile = await regularFilePath(requestedPath);
  if (existingFile !== undefined) {
    return fileResponse(existingFile, request.method === METHOD_HEAD);
  }

  if (extname(relativePath).length > 0) {
    return new Response("not found", { status: 404 });
  }

  const fallbackPath = resolve(root, INDEX_FILENAME);
  const fallbackFile = await regularFilePath(fallbackPath);
  return fallbackFile === undefined
    ? new Response("not found", { status: 404 })
    : fileResponse(fallbackFile, request.method === METHOD_HEAD);
}

function safeRelativePath(pathname: string): string | undefined {
  let decoded: string;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    return undefined;
  }
  if (decoded.includes("\0")) {
    return undefined;
  }
  if (decoded.split("/").includes("..")) {
    return undefined;
  }
  const relative = decoded.replace(/^\/+/, "");
  const normalized = resolve("/", relative).slice(1);
  if (normalized === ".." || normalized.startsWith(`..${sep}`)) {
    return undefined;
  }
  return normalized;
}

async function regularFilePath(path: string): Promise<string | undefined> {
  try {
    const metadata = await stat(path);
    return metadata.isFile() ? path : undefined;
  } catch (error: unknown) {
    if (isMissingPathError(error)) {
      return undefined;
    }
    throw error;
  }
}

async function fileResponse(path: string, headOnly: boolean): Promise<Response> {
  const body = headOnly ? null : await readFile(path);
  return new Response(body, {
    status: 200,
    headers: {
      [HEADER_CONTENT_TYPE]: CONTENT_TYPES.get(extname(path).toLowerCase()) ?? "application/octet-stream",
    },
  });
}

function isMissingPathError(error: unknown): boolean {
  return error instanceof Error
    && "code" in error
    && (error.code === "ENOENT" || error.code === "ENOTDIR");
}

function elapsedMilliseconds(startedAt: number): number {
  return Math.round((performance.now() - startedAt) * 100) / 100;
}
