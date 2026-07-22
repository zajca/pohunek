import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

const ENV_BACKEND_URL = "POHUNEK_VITE_BACKEND_URL";

export default defineConfig(() => {
  const backendUrl = process.env[ENV_BACKEND_URL];

  return {
    plugins: svelte(),
    build: {
      outDir: "dist"
    },
    ...(backendUrl === undefined
      ? {}
      : { server: {
          proxy: {
            "/api": {
              target: backendUrl
            },
            "/daemon": {
              target: backendUrl,
              ws: true
            }
          }
        } })
  };
});
