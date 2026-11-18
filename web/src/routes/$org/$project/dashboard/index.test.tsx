import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const DASHBOARD = "/acme/support-bot/dashboard";

const HEADINGS = ["Volume", "Quality", "Latency", "Cost"];
const NUMBERS = ["Traces", "Error rate", "p95 latency", "Cost", "Tokens"];
const CHARTS = [
  "Traces & errors",
  "Tokens by type",
  "Error rate",
  "Scores",
  "Latency",
  "LLM latency by model",
  "TTFT",
  "Cost by model",
  "Evaluation cost",
  "Top models by cost",
];

/** Records the bodies of the widget queries the page sends. */
function listenQueries() {
  const bodies: Schema<"WidgetQueryRequest">[] = [];
  const onStart = ({ request }: { request: Request }) => {
    if (new URL(request.url).pathname.endsWith("/dashboards/query")) {
      void request
        .clone()
        .json()
        .then((body) => bodies.push(body as Schema<"WidgetQueryRequest">));
    }
  };
  server.events.on("request:start", onStart);
  return {
    bodies,
    stop() {
      server.events.removeListener("request:start", onStart);
    },
  };
}

it("renders the four headings, the five numbers and every chart", async () => {
  const queries = listenQueries();
  try {
    await renderApp(DASHBOARD);
    for (const heading of HEADINGS) {
      expect(await screen.findByRole("heading", { level: 2, name: heading })).toBeInTheDocument();
    }
    for (const title of NUMBERS) {
      expect(screen.getAllByRole("region", { name: title }).length).toBeGreaterThan(0);
    }
    for (const title of CHARTS) {
      expect(screen.getAllByRole("region", { name: title }).length).toBeGreaterThan(0);
    }
    expect(await screen.findAllByText("5")).not.toHaveLength(0);
    await waitFor(() => {
      expect(queries.bodies.some((body) => body.bucket === "30m")).toBe(true);
    });
    expect(queries.bodies.every((body) => body.group_by === undefined)).toBe(true);
  } finally {
    queries.stop();
  }
});

it("has no sections to collapse", async () => {
  await renderApp(DASHBOARD);
  await screen.findByRole("heading", { level: 2, name: "Volume" });
  expect(screen.queryByRole("button", { name: "Volume" })).not.toBeInTheDocument();
  expect(localStorage.getItem("cetra.dashboard.collapsed.acme/support-bot")).toBeNull();
});

it("refetches when the range changes", async () => {
  const queries = listenQueries();
  try {
    const { router } = await renderApp(DASHBOARD);
    await screen.findByRole("heading", { level: 2, name: "Volume" });
    const before = queries.bodies.at(-1)?.from;
    fireEvent.click(screen.getByRole("button", { name: "Last 24 hours" }));
    fireEvent.click(await screen.findByRole("button", { name: "Last 1 hour" }));
    await waitFor(() => {
      const latest = queries.bodies.at(-1);
      expect(latest?.from).toBeTruthy();
      expect(latest?.from).not.toBe(before);
      const span = Date.parse(latest?.to ?? "") - Date.parse(latest?.from ?? "");
      expect(Math.abs(span - 3_600_000)).toBeLessThan(5_000);
    });
    expect(router.state.location.search).toMatchObject({ range: "1h" });
  } finally {
    queries.stop();
  }
});

it("sends group_by with every widget query when grouping by model", async () => {
  const queries = listenQueries();
  try {
    const { router } = await renderApp(DASHBOARD);
    await screen.findByRole("heading", { level: 2, name: "Volume" });
    fireEvent.change(screen.getByRole("combobox", { name: "Group by" }), {
      target: { value: "model" },
    });
    await waitFor(() => {
      expect(queries.bodies.some((body) => body.group_by === "model")).toBe(true);
    });
    expect(router.state.location.search).toMatchObject({ group: "model" });
  } finally {
    queries.stop();
  }
});

it("opens the traces of a clicked bucket with the dashboard's filter and flags", async () => {
  const { router } = await renderApp(
    "/acme/support-bot/dashboard?filter=status%20%3D%20'error'&eval=1&exp=1",
  );
  const card = await screen.findByRole("region", { name: "Traces & errors" });
  // The page re-renders while its queries settle, so the bucket is looked up again on each try.
  await waitFor(() => {
    const bucket = card.querySelector('[aria-label="Open bucket 2026-01-01T10:00:00.000Z"]');
    if (!(bucket instanceof Element)) {
      throw new Error("bucket not rendered yet");
    }
    fireEvent.click(bucket);
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

it("opens the last bucket from the keyboard", async () => {
  const { router } = await renderApp(DASHBOARD);
  const card = await screen.findByRole("region", { name: "Traces & errors" });
  const chart = await within(card).findByRole("group", { name: /^Traces & errors\./ });
  chart.focus();
  fireEvent.keyDown(chart, { key: "End" });
  fireEvent.keyDown(chart, { key: "Enter" });
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({
      from: "2026-01-01T10:30:00.000Z",
      to: "2026-01-01T10:40:00.000Z",
    });
  });
});

it("gives a score chart no bucket to open", async () => {
  await renderApp(DASHBOARD);
  const card = await screen.findByRole("region", { name: "Scores" });
  await within(card)
    .findByRole("group", { name: /^Scores\./ })
    .catch(() => undefined);
  expect(card.querySelectorAll('[aria-label^="Open bucket"]')).toHaveLength(0);
});
