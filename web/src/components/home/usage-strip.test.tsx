import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { UsageStrip, usageDayRows, usageTotals } from "@/components/home/usage-strip";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";

/** Renders the strip with a root route, so its link resolves. */
async function renderStrip() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const rootRoute = createRootRoute({ component: () => <UsageStrip org="acme" /> });
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  render(
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  await router.load();
}

describe("usageDayRows", () => {
  it("fills every day of the range", () => {
    const rows = usageDayRows({
      from: "2026-01-01",
      to: "2026-01-03",
      days: [
        { day: "2026-01-01", units: 5 },
        { day: "2026-01-03", units: 7 },
      ],
    });
    expect(rows.map((row) => row.day)).toEqual(["2026-01-01", "2026-01-02", "2026-01-03"]);
    expect(rows.map((row) => row.units)).toEqual([5, null, 7]);
    expect(rows[1].time - rows[0].time).toBe(86_400_000);
    expect(rows[2].time - rows[1].time).toBe(86_400_000);
  });

  it("is empty for an inverted range", () => {
    expect(usageDayRows({ from: "2026-01-03", to: "2026-01-01", days: [] })).toEqual([]);
  });
});

describe("usageTotals", () => {
  it("sums the days", () => {
    expect(
      usageTotals([
        { spans: 1200, scores: 40, judge_cost_usd: 0.42 },
        { spans: 800, scores: 10, judge_cost_usd: 0 },
      ]),
    ).toEqual({ spans: 2000, scores: 50, judgeCost: 0.42 });
  });
});

describe("UsageStrip", () => {
  it("shows the meter, the month totals, the daily chart and the Plan link", async () => {
    await renderStrip();

    const region = await screen.findByRole("region", { name: "Usage this month" });
    expect(within(region).getByRole("meter", { name: "Units this month" })).toBeInTheDocument();
    expect(within(region).getByText("2,000")).toBeInTheDocument();
    expect(within(region).getByText("50")).toBeInTheDocument();
    expect(within(region).getByText("$0.42")).toBeInTheDocument();
    expect(within(region).getByRole("img", { name: "Daily units" })).toBeInTheDocument();
    expect(within(region).getByRole("link", { name: "Plan" })).toHaveAttribute(
      "href",
      "/acme/settings/plan",
    );
  });

  it("says when the month has no usage", async () => {
    server.use(
      http.get("/api/v1/orgs/:org/usage", () =>
        HttpResponse.json({ from: "2026-01-01", to: "2026-01-03", days: [] }),
      ),
    );
    await renderStrip();

    expect(await screen.findByText("No usage this month.")).toBeInTheDocument();
  });

  it("hides itself when usage fails", async () => {
    server.use(http.get("/api/v1/orgs/:org/usage", () => problemResponse(500, "internal")));
    await renderStrip();

    await waitFor(() => {
      expect(document.querySelector("[data-slot='skeleton']")).toBeNull();
    });
    expect(screen.queryByRole("region", { name: "Usage this month" })).toBeNull();
  });

  it("hides itself when entitlements fail", async () => {
    server.use(http.get("/api/v1/orgs/:org/entitlements", () => problemResponse(500, "internal")));
    await renderStrip();

    await waitFor(() => {
      expect(document.querySelector("[data-slot='skeleton']")).toBeNull();
    });
    expect(screen.queryByRole("region", { name: "Usage this month" })).toBeNull();
  });
});
