import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const DASHBOARD = "/acme/support-bot/dashboard";
const TRACES = "/acme/support-bot/traces";

const TITLES = [
  "Traces & errors",
  "Error rate",
  "Latency",
  "LLM latency by model",
  "Cost by model",
  "Tokens by type",
  "TTFT",
  "Scores",
  "Evaluation cost",
  "Top models by cost",
];

function listenMetrics() {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/metrics")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  return {
    urls,
    stop() {
      server.events.removeListener("request:start", onStart);
    },
  };
}

it("renders the five KPIs and every chart card", async () => {
  const metrics = listenMetrics();
  try {
    await renderApp("/acme/support-bot/dashboard");
    expect(await screen.findByRole("heading", { name: "Dashboard" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Error rate: 20%" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "p95 latency: 4.20s" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cost: $0.00955" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Tokens: 5.7k" })).toBeInTheDocument();
    for (const title of TITLES) {
      expect(screen.getByRole("heading", { name: title })).toBeInTheDocument();
    }
    expect(
      screen.getByRole("region", { name: "Traces & errors: 5 traces, 1 error in range" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("region", { name: "TTFT: p50 600ms, p95 600ms in range" }),
    ).toBeInTheDocument();
    expect(metrics.urls.some((url) => url.searchParams.get("bucket") === "auto")).toBe(true);
    expect(metrics.urls.some((url) => url.searchParams.has("group_by"))).toBe(false);
  } finally {
    metrics.stop();
  }
});

it("refetches when the range changes", async () => {
  const metrics = listenMetrics();
  try {
    const { router } = await renderApp("/acme/support-bot/dashboard");
    await screen.findByRole("heading", { name: "Dashboard" });
    const before = metrics.urls.at(-1)?.searchParams.get("from");
    fireEvent.click(screen.getByRole("button", { name: "Last 24 hours" }));
    fireEvent.click(await screen.findByRole("button", { name: "Last 1 hour" }));
    await waitFor(() => {
      const latest = metrics.urls.at(-1);
      expect(latest?.searchParams.get("from")).toBeTruthy();
      expect(latest?.searchParams.get("from")).not.toBe(before);
      const from = Date.parse(latest?.searchParams.get("from") ?? "");
      const to = Date.parse(latest?.searchParams.get("to") ?? "");
      expect(Math.abs(to - from - 3_600_000)).toBeLessThan(5_000);
    });
    expect(router.state.location.search).toMatchObject({ range: "1h" });
  } finally {
    metrics.stop();
  }
});

it("sends group_by when grouping by model", async () => {
  const metrics = listenMetrics();
  try {
    const { router } = await renderApp("/acme/support-bot/dashboard");
    await screen.findByRole("heading", { name: "Dashboard" });
    fireEvent.change(screen.getByRole("combobox", { name: "Group by" }), {
      target: { value: "model" },
    });
    await waitFor(() => {
      expect(metrics.urls.some((url) => url.searchParams.get("group_by") === "model")).toBe(true);
    });
    expect(router.state.location.search).toMatchObject({ group: "model" });
  } finally {
    metrics.stop();
  }
});

it("opens traces for the clicked bucket", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/dashboard?filter=status%20%3D%20'error'&eval=1&exp=1",
  );
  const card = await screen.findByRole("region", { name: /Traces & errors: / });
  const bucket = await waitFor(() => {
    const found = card.querySelector('[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]');
    expect(found).toBeInstanceOf(Element);
    return found;
  });
  if (!(bucket instanceof Element)) {
    throw new Error("missing bucket");
  }
  fireEvent.click(bucket);
  await waitFor(() => {
    expect(router.state.location.pathname).toContain("/traces");
  });
  expect(router.state.location.search).toMatchObject({
    from: "2026-01-01T10:00:00.000Z",
    to: "2026-01-01T10:10:00.000Z",
    filter: "status = 'error'",
    eval: 1,
    exp: 1,
  });
  expect(router.state.location.search).not.toHaveProperty("range");
});

it("gives each chart one tab stop", async () => {
  await renderApp("/acme/support-bot/dashboard");
  const card = await screen.findByRole("region", { name: /Traces & errors: / });
  await waitFor(() => {
    expect(card.querySelector('[aria-label^="Open bucket"]')).toBeInstanceOf(Element);
  });
  const stops = [...card.querySelectorAll<HTMLElement>("button, [tabindex]")].filter(
    (element) => element.tabIndex >= 0,
  );
  expect(stops).toHaveLength(1);
  expect(stops[0]).toHaveAttribute("role", "group");
  for (const point of document.querySelectorAll<HTMLElement>("foreignObject button")) {
    expect(point.tabIndex).toBe(-1);
  }
});

it("opens a bucket from the keyboard", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/dashboard?filter=status%20%3D%20'error'&eval=1&exp=1",
  );
  const card = await screen.findByRole("region", { name: /Traces & errors: / });
  const chart = await within(card).findByRole("group", { name: /^Traces & errors\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "ArrowRight" });
  fireEvent.keyDown(chart, { key: "ArrowRight" });
  const live = card.querySelector('[aria-live="polite"]');
  expect(live?.textContent).toContain("Traces");
  fireEvent.keyDown(chart, { key: "Enter" });
  await waitFor(() => {
    expect(router.state.location.pathname).toContain("/traces");
  });
  expect(router.state.location.search).toMatchObject({
    from: "2026-01-01T10:10:00.000Z",
    to: "2026-01-01T10:20:00.000Z",
    filter: "status = 'error'",
    eval: 1,
    exp: 1,
  });
});

it("jumps to the last bucket with End", async () => {
  const { router } = await renderApp("/acme/support-bot/dashboard");
  const card = await screen.findByRole("region", { name: /Traces & errors: / });
  const chart = await within(card).findByRole("group", { name: /^Traces & errors\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "End" });
  fireEvent.keyDown(chart, { key: "Enter" });
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({
      from: "2026-01-01T10:50:00.000Z",
      to: "2026-01-01T11:00:00.000Z",
    });
  });
});

it("reuses the cached dashboard when it is shown again within the same minute", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(new Date("2026-01-01T10:00:02Z"));
  const metrics = listenMetrics();
  try {
    const { router } = await renderApp(DASHBOARD);
    expect(await screen.findByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    await act(() => router.navigate({ href: TRACES }));
    vi.setSystemTime(new Date("2026-01-01T10:00:08Z"));
    await act(() => router.navigate({ href: DASHBOARD }));
    expect(screen.getByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Loading dashboard" })).not.toBeInTheDocument();
    expect(metrics.urls).toHaveLength(1);
  } finally {
    metrics.stop();
    vi.useRealTimers();
  }
});

it("shows the previous dashboard at once and refetches two minutes later", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(new Date("2026-01-01T10:00:02Z"));
  const metrics = listenMetrics();
  try {
    const { router, queryClient } = await renderApp(DASHBOARD);
    expect(await screen.findByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    const [cached] = queryClient
      .getQueryCache()
      .findAll({ queryKey: ["get", "/api/v1/projects/{project}/metrics"] });
    const body = cached?.state.data as Schema<"MetricsResponse">;
    server.use(
      http.get("*/api/v1/projects/*/metrics", () =>
        HttpResponse.json({
          ...body,
          kpis: body.kpis.map((kpi) => (kpi.metric === "traces" ? { ...kpi, value: 9 } : kpi)),
        }),
      ),
    );
    await act(() => router.navigate({ href: TRACES }));
    vi.setSystemTime(new Date("2026-01-01T10:02:02Z"));
    await act(() => router.navigate({ href: DASHBOARD }));
    expect(screen.getByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Loading dashboard" })).not.toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Traces: 9" })).toBeInTheDocument();
    expect(metrics.urls).toHaveLength(2);
  } finally {
    metrics.stop();
    vi.useRealTimers();
  }
});

it("fetches again when refresh is pressed within the same minute", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(new Date("2026-01-01T10:00:02Z"));
  const metrics = listenMetrics();
  try {
    await renderApp(DASHBOARD);
    expect(await screen.findByRole("button", { name: "Traces: 5" })).toBeInTheDocument();
    expect(metrics.urls).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => {
      expect(metrics.urls).toHaveLength(2);
    });
  } finally {
    metrics.stop();
    vi.useRealTimers();
  }
});

it("shows how long ago the dashboard data was updated", async () => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(new Date("2026-01-01T10:00:02Z"));
  try {
    await renderApp(DASHBOARD);
    expect(await screen.findByText(/^Updated \d+s ago$/)).toBeInTheDocument();
    act(() => {
      vi.setSystemTime(new Date("2026-01-01T10:03:02Z"));
      vi.advanceTimersByTime(10_000);
    });
    expect(await screen.findByText("Updated 3 min ago")).toBeInTheDocument();
  } finally {
    vi.useRealTimers();
  }
});

it("shows the loading skeleton with every chart title while metrics are pending", async () => {
  server.use(http.get("*/api/v1/projects/*/metrics", () => new Promise<never>(() => {})));
  await renderApp(DASHBOARD);
  const status = await screen.findByRole("status", { name: "Loading dashboard" });
  for (const title of TITLES) {
    expect(within(status).getByRole("region", { name: title })).toHaveAttribute(
      "aria-busy",
      "true",
    );
  }
  expect(screen.queryByRole("button", { name: /^Traces:/ })).not.toBeInTheDocument();
});

it("keeps the dashboard mounted while another project page is open", async () => {
  const { router } = await renderApp(DASHBOARD);
  const tile = await screen.findByRole("button", { name: "Traces: 5" });
  await act(() => router.navigate({ href: TRACES }));
  expect(screen.queryByRole("button", { name: "Traces: 5" })).not.toBeInTheDocument();
  expect(tile.isConnected).toBe(true);
  await act(() => router.navigate({ href: DASHBOARD }));
  expect(screen.getByRole("button", { name: "Traces: 5" })).toBe(tile);
});
