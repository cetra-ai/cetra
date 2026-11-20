import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, URL } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import tanstackRouter from "@tanstack/router-plugin/vite";
import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const workerPath = fileURLToPath(new URL("./msw/mockServiceWorker.js", import.meta.url));

/** The `version` of the `[workspace.package]` table in the root Cargo.toml. */
function workspaceVersion(manifest: string): string {
  const section = /^\[workspace\.package\][ \t]*\r?\n([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(manifest);
  const version = section ? /^version\s*=\s*"([^"]+)"/m.exec(section[1] ?? "") : null;
  if (!version?.[1]) {
    throw new Error("no version in [workspace.package] of the root Cargo.toml");
  }
  return version[1];
}

const cetraVersion = workspaceVersion(readFileSync(join(repoRoot, "Cargo.toml"), "utf8"));

/**
 * Serves MSW's worker script to the dev server, and emits it into the `demo` build (D-C-22). The
 * production build, embedded in the binary, never contains it (`scripts/check-dist.mjs`).
 */
function mockServiceWorker(mode: string): Plugin {
  return {
    name: "cetra-mock-service-worker",
    generateBundle() {
      if (mode === "demo") {
        this.emitFile({
          type: "asset",
          fileName: "mockServiceWorker.js",
          source: readFileSync(workerPath),
        });
      }
    },
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const url = request.url?.split("?")[0];
        if (url !== "/mockServiceWorker.js") {
          next();
          return;
        }
        response.setHeader("Content-Type", "text/javascript");
        response.end(readFileSync(workerPath));
      });
    },
  };
}

export default defineConfig(({ mode }) => ({
  plugins: [
    mockServiceWorker(mode),
    tanstackRouter({ target: "react", autoCodeSplitting: true }),
    react(),
    tailwindcss(),
  ],
  define: {
    __CETRA_VERSION__: JSON.stringify(cetraVersion),
  },
  build: {
    // The `demo` build (D-C-22) goes beside `dist/`, so it can never be the one the binary embeds.
    outDir: mode === "demo" ? "dist-demo" : "dist",
    // The UI's CSP (spec 12) has no `data:` source for fonts or scripts: every asset is a file.
    assetsInlineLimit: 0,
  },
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  server: {
    fs: {
      allow: [repoRoot],
    },
    ...(mode === "server"
      ? {
          proxy: {
            "/api": "http://localhost:8080",
            "/v1": "http://localhost:8080",
          },
        }
      : {}),
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    restoreMocks: true,
    testTimeout: 15_000,
    hookTimeout: 30_000,
  },
}));
