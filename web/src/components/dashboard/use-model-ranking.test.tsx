import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { expect, it } from "vitest";
import { useModelRanking } from "./use-model-ranking";
import type { WidgetContext } from "./widget";

const CONTEXT: WidgetContext = {
  org: "acme",
  project: "support-bot",
  from: "2026-01-01T10:00:00.000Z",
  to: "2026-01-01T11:00:00.000Z",
  includeEval: false,
  includeExperiments: false,
};

it("has no update time before the first ranking, and one once it arrives", async () => {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  function wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  }
  const { result } = renderHook(() => useModelRanking(CONTEXT), { wrapper });
  // Before the first answer the time is unknown, not the epoch.
  expect(result.current.updatedAt).toBeUndefined();
  await waitFor(() => {
    expect(result.current.updatedAt).toBeGreaterThan(0);
  });
});
