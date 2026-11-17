import { act, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import { server } from "@/mocks/node";
import { FIRING_RULE_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import rulesFixture from "../../../../../../fixtures/api/static/alert-rules.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";
import projectEventsFixture from "../../../../../../fixtures/api/static/project-alert-events.json";

const PAGE = "/acme/support-bot/alerts";
const RULES = "/api/v1/projects/:project/alert-rules";
const EVENTS = "/api/v1/projects/:project/alert-events";
const RULE = "/api/v1/projects/:project/alert-rules/:id";
const METRICS = "/api/v1/projects/:project/metrics";

afterEach(() => {
  vi.useRealTimers();
});

/** A metrics response with one error-rate series. */
function errorRateMetrics(values: (number | undefined)[]) {
  return {
    bucket_ns: 300_000_000_000,
    from: "2026-01-01T00:00:00.000Z",
    to: "2026-01-02T00:00:00.000Z",
    kpis: [],
    series: [
      {
        metric: "error_rate",
        points: values.map((value, index) => ({
          time: new Date(Date.UTC(2026, 0, 1, index)).toISOString(),
          ...(value === undefined ? {} : { value }),
        })),
      },
    ],
    top_models: [],
  };
}

it("lists the rules with their state", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(3);

  const high = within(table).getByRole("row", { name: /^High error rate\b/ });
  expect(high).toHaveTextContent("Firing");
  expect(high).toHaveTextContent("Error rate > 5.0% over 5m");
  expect(high).toHaveTextContent("12.5%");
  expect(within(high).getByText("Team Slack")).toBeInTheDocument();
  expect(within(high).getByText("Pager webhook")).toBeInTheDocument();
  expect(within(high).getByRole("switch", { name: "Enable High error rate" })).toBeChecked();

  const daily = within(table).getByRole("row", { name: /^Daily cost\b/ });
  expect(daily).toHaveTextContent("Disabled");

  const helpfulness = within(table).getByRole("row", { name: /^Low helpfulness\b/ });
  expect(helpfulness).toHaveTextContent("No data");
});

it("links to the rule page", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getByRole("link", { name: "High error rate" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts/${FIRING_RULE_ID}`,
  );
});

it("shows a rule that was never evaluated as never", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  const daily = within(table).getByRole("row", { name: /^Daily cost\b/ });
  expect(within(daily).getByText("Never")).toBeInTheDocument();
});

it("flags a failed evaluation", async () => {
  server.use(
    http.get(RULES, () =>
      HttpResponse.json({
        ...rulesFixture,
        data: [{ ...rulesFixture.data[1], last_error: "query timed out" }],
      }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByLabelText("Evaluation failed")).toBeInTheDocument();
});

it("shows the empty state", async () => {
  server.use(http.get(RULES, () => HttpResponse.json({ data: [], next_cursor: null })));

  await renderApp(PAGE);

  expect(await screen.findByRole("heading", { name: "No alert rules yet" })).toBeInTheDocument();
});

it("shows the Alerts entry in the sidebar as current", async () => {
  await renderApp(PAGE);

  await screen.findByRole("table", { name: "Alert rules" });
  expect(screen.getByRole("link", { name: "Alerts" })).toHaveAttribute("aria-current", "page");
});

it("shows the forbidden state", async () => {
  server.use(http.get(RULES, () => problemResponse(403, "forbidden")));

  await renderApp(PAGE);

  expect(
    await screen.findByText("You don't have access to this project's alert rules."),
  ).toBeInTheDocument();
});

it("links New rule to the editor", async () => {
  await renderApp(PAGE);

  expect(await screen.findByRole("link", { name: "New rule" })).toHaveAttribute(
    "href",
    "/acme/support-bot/alerts/new",
  );
});

it("disables New rule for viewers", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(PAGE);

  expect(await screen.findByRole("button", { name: "New rule" })).toBeDisabled();
  expect(screen.queryByRole("link", { name: "New rule" })).toBeNull();
});

it("counts the rules per bucket, a disabled rule only as Disabled", async () => {
  await renderApp(PAGE);

  await screen.findByRole("table", { name: "Alert rules" });
  expect(screen.getByRole("button", { name: "Firing 1" })).toHaveAttribute("aria-pressed", "false");
  expect(screen.getByRole("button", { name: "No data 1" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "OK 0" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Disabled 1" })).toBeInTheDocument();
});

it("filters the table by a bucket through the state search param and clears it again", async () => {
  const { router, user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Alert rules" });

  await user.click(screen.getByRole("button", { name: "Firing 1" }));

  expect(router.state.location.search).toMatchObject({ state: "firing" });
  expect(screen.getByRole("button", { name: "Firing 1" })).toHaveAttribute("aria-pressed", "true");
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(1);
  expect(within(table).getByRole("link", { name: "High error rate" })).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Firing 1" }));

  expect(router.state.location.search.state).toBeUndefined();
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(3);
});

it("reads the bucket filter from the URL", async () => {
  await renderApp(`${PAGE}?state=disabled`);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(1);
  expect(within(table).getByRole("link", { name: "Daily cost" })).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Disabled 1" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});

it("sorts firing first, then no data, OK, disabled, then by name", async () => {
  server.use(
    http.get(RULES, () =>
      HttpResponse.json({
        ...rulesFixture,
        data: [
          rulesFixture.data[0],
          { ...rulesFixture.data[2], name: "Zeta" },
          { ...rulesFixture.data[2], id: "x-2", name: "Alpha", state: "ok" },
          rulesFixture.data[1],
        ],
      }),
    ),
  );

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  const names = within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getAllByRole("cell")[0]?.textContent);
  expect(names).toEqual(["High error rate", "Zeta", "Alpha", "Daily cost"]);
});

it("shows how long a firing rule has been firing", async () => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(new Date("2026-01-01T12:00:00Z"));
  server.use(
    http.get(RULES, () =>
      HttpResponse.json({
        ...rulesFixture,
        data: [
          { ...rulesFixture.data[1], last_fired_at: "2026-01-01T09:46:00Z" },
          { ...rulesFixture.data[2], state: "ok" },
        ],
      }),
    ),
  );

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getByText("for 2h 14m")).toBeInTheDocument();
  expect(within(table).getAllByText(/^for /)).toHaveLength(1);
});

it("shows no duration for a firing rule without a fire time", async () => {
  server.use(
    http.get(RULES, () =>
      HttpResponse.json({
        ...rulesFixture,
        data: [{ ...rulesFixture.data[1], last_fired_at: undefined }],
      }),
    ),
  );

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getByRole("link", { name: "High error rate" })).toBeInTheDocument();
  expect(within(table).queryByText(/^for /)).toBeNull();
});

it("draws the trend with the threshold once the row is visible", async () => {
  let requests = 0;
  server.use(
    http.get(METRICS, () => {
      requests += 1;
      return HttpResponse.json(errorRateMetrics([0.01, 0.2, undefined, 0.08]));
    }),
  );
  const callbacks: IntersectionObserverCallback[] = [];
  const original = globalThis.IntersectionObserver;
  globalThis.IntersectionObserver = class {
    constructor(callback: IntersectionObserverCallback) {
      callbacks.push(callback);
    }
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
    takeRecords(): IntersectionObserverEntry[] {
      return [];
    }
  } as unknown as typeof IntersectionObserver;
  try {
    await renderApp(PAGE);
    await screen.findByRole("table", { name: "Alert rules" });
    expect(requests).toBe(0);
    expect(screen.queryByRole("img", { name: /^Trend of/ })).toBeNull();

    act(() => {
      for (const callback of callbacks) {
        callback([{ isIntersecting: true } as IntersectionObserverEntry], {} as never);
      }
    });

    expect(
      await screen.findByRole("img", {
        name: "Trend of High error rate over the last 5m",
      }),
    ).toBeInTheDocument();
    expect(requests).toBeGreaterThan(0);
  } finally {
    globalThis.IntersectionObserver = original;
  }
});

it("shows a dash in the trend when the metrics request fails", async () => {
  server.use(http.get(METRICS, () => problemResponse(500, "internal")));

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  await waitFor(() =>
    expect(within(table).getByRole("row", { name: /^High error rate\b/ })).toHaveTextContent("—"),
  );
  expect(screen.queryByRole("img", { name: /^Trend of/ })).toBeNull();
});

it("shows None with a tooltip for a rule without channels", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  const none = within(table).getByText("None");
  expect(none).toHaveAttribute(
    "title",
    "Not sent anywhere: this rule only shows here and notifies nobody.",
  );
});

it("shows at most two channels, then the count of the rest", async () => {
  server.use(
    http.get(RULES, () =>
      HttpResponse.json({
        ...rulesFixture,
        data: [
          {
            ...rulesFixture.data[1],
            channel_ids: [
              "01900000-0000-7000-8000-000000000500",
              "01900000-0000-7000-8000-000000000501",
              "01900000-0000-7000-8000-000000000502",
            ],
          },
        ],
      }),
    ),
  );

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  const row = within(table).getByRole("row", { name: /^High error rate\b/ });
  expect(within(row).getByText("Team Slack")).toBeInTheDocument();
  expect(within(row).getByText("Pager webhook")).toBeInTheDocument();
  expect(within(row).queryByText("On-call email")).toBeNull();
  expect(within(row).getByText("+1")).toHaveAttribute("title", "On-call email");
  expect(within(row).getByRole("img", { name: "Slack" })).toBeInTheDocument();
  expect(within(row).getByRole("img", { name: "Webhook" })).toBeInTheDocument();
});

it("saves the enabled flag from the row switch", async () => {
  let body: unknown;
  server.use(
    http.patch(RULE, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...rulesFixture.data[1], enabled: false });
    }),
  );

  const { user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Alert rules" });
  await user.click(within(table).getByRole("switch", { name: "Enable High error rate" }));

  await waitFor(() => expect(body).toEqual({ enabled: false }));
});

it("shows the plan limit under the row switch", async () => {
  server.use(
    http.patch(RULE, () =>
      problemResponse(403, "plan_limit", { detail: "Your plan allows 3 enabled rules." }),
    ),
  );

  const { user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Alert rules" });
  await user.click(within(table).getByRole("switch", { name: "Enable Daily cost" }));

  expect(
    await within(table).findByText("Upgrade your plan or disable another rule."),
  ).toBeInTheDocument();
});

it("disables the row switches for viewers", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getByRole("switch", { name: "Enable High error rate" })).toBeDisabled();
  expect(within(table).getByRole("switch", { name: "Enable Daily cost" })).toBeDisabled();
});

const HIGH_ID = "01900000-0000-7000-8000-000000000510";
const LOW_ID = "01900000-0000-7000-8000-000000000511";
const [firedEvent, resolvedEvent] = projectEventsFixture.data;

it("lists the recent events with the names of their rules", async () => {
  server.use(
    http.get(EVENTS, () =>
      HttpResponse.json({
        data: [
          firedEvent,
          { ...resolvedEvent, id: "01900000-0000-7000-8000-000000000530", rule_id: LOW_ID },
        ],
        next_cursor: null,
      }),
    ),
  );

  await renderApp(PAGE);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  expect(within(events).getAllByRole("row").slice(1)).toHaveLength(2);
  expect(within(events).getByRole("link", { name: "High error rate" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts/${HIGH_ID}`,
  );
  expect(within(events).getByRole("link", { name: "Low helpfulness" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts/${LOW_ID}`,
  );
  expect(within(events).getByText("Fired")).toBeInTheDocument();
  expect(within(events).getByText("Resolved")).toBeInTheDocument();
});

it("shows the value against the threshold of the rule's metric", async () => {
  await renderApp(PAGE);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  expect(within(events).getByText("12.5% > 5.0%")).toBeInTheDocument();
});

it("shows Deleted rule for an event of a rule that is gone", async () => {
  server.use(
    http.get(EVENTS, () =>
      HttpResponse.json({
        data: [{ ...firedEvent, rule_id: "01900000-0000-7000-8000-0000000009ff" }],
        next_cursor: null,
      }),
    ),
  );

  await renderApp(PAGE);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  expect(within(events).getByText("Deleted rule")).toBeInTheDocument();
  expect(within(events).queryByRole("link")).toBeNull();
});

it("styles the deliveries in destructive colour when one failed", async () => {
  await renderApp(PAGE);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  const failed = within(events).getByText("1 of 2 sent");
  expect(failed).toHaveClass("text-destructive");
  expect(failed).toHaveAttribute("title", "Pager webhook: the endpoint answered 500");
  expect(within(events).getAllByText("1 of 1 sent")[0]).not.toHaveClass("text-destructive");
});

it("loads the next page of events", async () => {
  server.use(
    http.get(EVENTS, ({ request }) => {
      if (new URL(request.url).searchParams.get("cursor") === "c1") {
        return HttpResponse.json({
          data: [{ ...resolvedEvent, id: "01900000-0000-7000-8000-000000000531", rule_id: LOW_ID }],
          next_cursor: null,
        });
      }
      return HttpResponse.json({ ...projectEventsFixture, next_cursor: "c1" });
    }),
  );

  const { user } = await renderApp(PAGE);
  const events = await screen.findByRole("table", { name: "Recent alert events" });
  await user.click(screen.getByRole("button", { name: "Load more" }));

  expect(await within(events).findByRole("link", { name: "Low helpfulness" })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
});

it("shows the empty text when there are no events", async () => {
  server.use(http.get(EVENTS, () => HttpResponse.json({ data: [], next_cursor: null })));

  await renderApp(PAGE);

  expect(
    await screen.findByText("No events yet. Events appear when a rule fires or resolves."),
  ).toBeInTheDocument();
});

it("shows the recent events whatever the bucket filter", async () => {
  await renderApp(`${PAGE}?state=disabled`);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  expect(within(events).getAllByRole("row").slice(1)).toHaveLength(3);
});
