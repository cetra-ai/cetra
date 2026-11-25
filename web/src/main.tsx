import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import { createAppQueryClient } from "@/api/client";
import {
  applyDensity,
  applyPalette,
  applyResolvedTheme,
  readStoredDensity,
  readStoredPalette,
  readStoredTheme,
  resolveTheme,
  ThemeProvider,
} from "@/components/theme-provider";
import { createAppRouter } from "@/router";
import "./styles.css";

async function main(): Promise<void> {
  applyResolvedTheme(resolveTheme(readStoredTheme()));
  applyPalette(readStoredPalette());
  applyDensity(readStoredDensity());

  // The dev server mocks the API unless it proxies a real server; the `demo` build (D-C-22) always
  // does. Both conditions are constants at build time, so the production build has no mocks.
  if (
    import.meta.env.MODE === "demo" ||
    (import.meta.env.DEV && import.meta.env.MODE !== "server")
  ) {
    const { isCommonAssetRequest } = await import("msw");
    const { HttpNetworkFrame } = await import("msw/experimental");
    const [{ worker }, { readMockMode }] = await Promise.all([
      import("./mocks/browser"),
      import("./mocks/mock-mode"),
    ]);
    if (readMockMode() === "large") {
      const [
        { createDataset, SUPPORT_PROFILE },
        { CODING_PROFILE, VOICE_PROFILE, EXTRACTION_PROFILE },
        { largeHandlers },
        { STORY_PROJECTS, projectHandlers },
      ] = await Promise.all([
        import("./mocks/generated/dataset"),
        import("./mocks/generated/profiles"),
        import("./mocks/generated/handlers"),
        import("./mocks/projects"),
      ]);
      const profiles = {
        support: SUPPORT_PROFILE,
        coding: CODING_PROFILE,
        voice: VOICE_PROFILE,
        extraction: EXTRACTION_PROFILE,
      };
      const now = Date.now();
      worker.use(...projectHandlers);
      for (const project of STORY_PROJECTS) {
        const dataset = createDataset({
          seed: 1,
          now,
          traces: project.story === "support" ? 5_000 : 2_000,
          profile: profiles[project.story],
        });
        worker.use(
          ...largeHandlers(dataset, {
            project: project.ref,
            datasets: project.story === "support",
          }),
        );
      }
    }
    await worker.start({
      quiet: import.meta.env.MODE === "demo",
      // The document request for `/` is not an API call. Warn on anything else
      // that the fixtures do not cover. Common assets stay quiet, as with "warn".
      onUnhandledFrame({ frame, defaults }) {
        if (!(frame instanceof HttpNetworkFrame)) {
          defaults.warn();
          return;
        }
        const { request } = frame.data;
        if (request.mode === "navigate" || isCommonAssetRequest(request)) {
          return;
        }
        defaults.warn();
      },
    });
  }

  const root = document.getElementById("root");
  if (!root) {
    throw new Error("Missing #root");
  }

  const queryClient = createAppQueryClient();
  const router = createAppRouter({ queryClient });

  createRoot(root).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <ThemeProvider>
          <RouterProvider router={router} />
          {import.meta.env.MODE === "demo" ? (
            <div
              role="status"
              className="pointer-events-none fixed bottom-3 left-1/2 z-50 -translate-x-1/2 rounded-full border bg-muted px-3 py-1 text-caption text-muted-foreground shadow-sm"
            >
              Demo, sample data
            </div>
          ) : null}
        </ThemeProvider>
      </QueryClientProvider>
    </StrictMode>,
  );
}

void main();
