import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const DAY_MS = 24 * 60 * 60 * 1000;

const twoVersions = {
  from: "2026-11-13T10:00:00.000Z",
  to: "2026-11-20T10:00:00.000Z",
  versions: [
    {
      version: 2,
      span_count: 4,
      trace_count: 3,
      error_count: 1,
      cost_total: 0.4,
      cost_avg: 0.1,
      latency_p50_ms: 120.5,
      latency_p95_ms: 300,
      input_tokens_avg: 1200,
      output_tokens_avg: 300,
      scores: [{ name: "quality", count: 4, mean: 0.5 }],
    },
    {
      version: 1,
      span_count: 2,
      trace_count: 2,
      error_count: 0,
      cost_total: 0.2,
      cost_avg: 0.1,
      latency_p50_ms: 80,
      latency_p95_ms: 90,
      input_tokens_avg: 900,
      output_tokens_avg: 200,
      scores: [{ name: "quality", count: 2, mean: 0.75 }],
    },
  ],
};

/** The row of a version in the metrics table. */
function rowOf(table: HTMLElement, version: string): HTMLElement {
  const row = within(table).getByText(version).closest("tr");
  if (row === null) {
    throw new Error(`no row for ${version}`);
  }
  return row;
}

it("shows the per-version table", async () => {
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt/metrics", () =>
      HttpResponse.json(twoVersions),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply?tab=metrics");

  const table = await screen.findByRole("table", { name: "Metrics by version" });
  const row = rowOf(table, "v2");
  expect(within(row).getByText("25%")).toBeInTheDocument();
  expect(within(row).getByText("121ms")).toBeInTheDocument();
  expect(within(row).getByText("0.50")).toBeInTheDocument();
  expect(within(row).getByText("n = 4")).toBeInTheDocument();
  expect(within(rowOf(table, "v1")).getByText("0%")).toBeInTheDocument();
  expect(within(table).getByRole("columnheader", { name: "quality" })).toBeInTheDocument();
});

it("changes the window of the query", async () => {
  const user = userEvent.setup();
  const windows: { from: string; to: string }[] = [];
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt/metrics", ({ request }) => {
      const url = new URL(request.url);
      windows.push({
        from: url.searchParams.get("from") ?? "",
        to: url.searchParams.get("to") ?? "",
      });
      return HttpResponse.json({ ...twoVersions, versions: [] });
    }),
  );
  await renderApp("/acme/support-bot/prompts/support-reply?tab=metrics");
  await waitFor(() => expect(windows).toHaveLength(1));

  await user.click(screen.getByRole("combobox", { name: "Window" }));
  await user.click(await screen.findByRole("option", { name: "30 days" }));

  await waitFor(() => expect(windows).toHaveLength(2));
  const last = windows[1];
  expect(last).toBeDefined();
  expect(Date.parse(last?.to ?? "") - Date.parse(last?.from ?? "")).toBe(30 * DAY_MS);
});

it("links each version to its traces on the same window", async () => {
  const user = userEvent.setup();
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt/metrics", () =>
      HttpResponse.json(twoVersions),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply?tab=metrics");

  const table = await screen.findByRole("table", { name: "Metrics by version" });
  await user.click(screen.getByRole("combobox", { name: "Window" }));
  await user.click(await screen.findByRole("option", { name: "30 days" }));

  const link = within(rowOf(table, "v2")).getByRole("link", { name: "Traces" });
  const search = new URL(link.getAttribute("href") ?? "", "http://x").searchParams;
  expect(search.get("range")).toBe("30d");
  expect(search.get("filter")).toBe("prompts @> ['support-reply@2']");
});

it("says so when no span is linked in the window", async () => {
  server.use(
    http.get("/api/v1/projects/:project/prompts/:prompt/metrics", () =>
      HttpResponse.json({ ...twoVersions, versions: [] }),
    ),
  );
  await renderApp("/acme/support-bot/prompts/support-reply?tab=metrics");

  expect(
    await screen.findByText(
      "No linked spans in this window. Link spans with context(prompt=…) in the SDK.",
    ),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Metrics by version" })).not.toBeInTheDocument();
});
