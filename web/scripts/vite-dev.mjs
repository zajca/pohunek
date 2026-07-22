import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const FRONTEND_ROOT = join(SCRIPT_DIR, "..", "frontend");
const LOOPBACK_HOST = "127.0.0.1";
const DYNAMIC_PORT = 0;
const READY_PREFIX = "POHUNEK_VITE_READY ";
const DEV_SIGNALS = ["SIGINT", "SIGTERM"];

let vite;
let shutdownTask;

function shutdown() {
  shutdownTask ??= vite?.close() ?? Promise.resolve();
  return shutdownTask;
}

try {
  vite = await createServer({
    root: FRONTEND_ROOT,
    appType: "spa",
    clearScreen: false,
    server: {
      host: LOOPBACK_HOST,
      port: DYNAMIC_PORT,
      strictPort: false,
    },
  });
  await vite.listen();

  const address = vite.httpServer?.address();
  if (address === null || address === undefined || typeof address === "string") {
    throw new Error("Vite dev server did not expose a TCP address");
  }
  process.stdout.write(`${READY_PREFIX}${JSON.stringify({
    url: `http://${LOOPBACK_HOST}:${address.port}`,
  })}\n`);

  for (const signal of DEV_SIGNALS) {
    process.once(signal, () => {
      void shutdown().catch((error) => {
        console.error(error);
        process.exitCode = 1;
      });
    });
  }
} catch (error) {
  await shutdown();
  throw error;
}
