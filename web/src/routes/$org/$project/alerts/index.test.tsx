import { act, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import { server } from "@/mocks/node";
import { FIRING_RULE_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import eventsFixture from "../../../../../../fixtures/api/static/alert-events.json";
import ruleFixture from "../../../../../../fixtures/api/static/alert-rule.json";
import rulesFixture from "../../../../../../fixtures/api/static/alert-rules.json";
import entitlementsFixture from "../../../../../../fixtures/api/static/org-entitlements.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";
import projectEventsFixture from "../../../../../../fixtures/api/static/project-alert-events.json";

const PAGE = "/acme/support-bot/alerts";
const RULES = "/api/v1/projects/:project/alert-rules";
const EVENTS = "/api/v1/projects/:project/alert-events";
const RULE = "/api/v1/projects/:project/alert-rules/:id";
const RULE_EVENTS = "/api/v1/projects/:project/alert-rules/:id/events";
const WIDGET = "/api/v1/projects/:project/dashboards/query";
const CANNOT_MANAGE = "Only project members and above can manage alerts";

afterEach(() => {
  vi.useRealTimers();
});

/** A widget query answer with one error-rate series. */
function errorRateAnswer(values: (number | undefined)[]) {
  return {
    shape: "series",
    bucket_ns: 300_000_000_000,
    series: [
      {
        measure: 0,
        other: false,
        points: values.flatMap((value, index) =>
          value === undefined
            ? []
            : [{ time: new Date(Date.UTC(2026, 0, 1, index)).toISOString(), value, rows: 1 }],
        ),
      },
    ],
  };
}

it("lists the rules with their state", async () => {
  await renderApp(PAGE);
  expect(
    screen.getAllByRole("heading", { level: 1 }).map((heading) => heading.textContent),
  ).toEqual(["Alerts"]);

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

it("links the rule name to the rule in the panel", async () => {
  await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  expect(within(table).getByRole("link", { name: "High error rate" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts?rule=${FIRING_RULE_ID}`,
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
    http.post(WIDGET, () => {
      requests += 1;
      return HttpResponse.json(errorRateAnswer([0.01, 0.2, undefined, 0.08]));
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

it("shows a dash in the trend when the widget query fails", async () => {
  server.use(http.post(WIDGET, () => problemResponse(500, "internal")));

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
    `/acme/support-bot/alerts?rule=${HIGH_ID}`,
  );
  expect(within(events).getByRole("link", { name: "Low helpfulness" })).toHaveAttribute(
    "href",
    `/acme/support-bot/alerts?rule=${LOW_ID}`,
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

const DAILY_ID = "01900000-0000-7000-8000-000000000512";
const PANEL = { name: "Alert rule" };

it("opens the rule in the panel when its row is clicked", async () => {
  const { router, user } = await renderApp(PAGE);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  await user.click(within(table).getByText("Never"));

  expect(router.state.location.search).toMatchObject({ rule: DAILY_ID });
  expect(await screen.findByRole("region", PANEL)).toBeInTheDocument();
});

it("opens the rule named in the URL", async () => {
  server.use(http.get(RULE, () => HttpResponse.json(rulesFixture.data[2])));

  await renderApp(`${PAGE}?rule=${LOW_ID}`);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Low helpfulness")).toBeInTheDocument();
});

it("closes the panel and removes the rule param", async () => {
  const { router, user } = await renderApp(`${PAGE}?rule=${HIGH_ID}`);

  const panel = await screen.findByRole("region", PANEL);
  await user.click(within(panel).getByRole("button", { name: "Close" }));

  expect(router.state.location.search.rule).toBeUndefined();
  await waitFor(() => expect(screen.queryByRole("region", PANEL)).toBeNull());
});

it("moves the open rule with the arrow keys", async () => {
  const { router, user } = await renderApp(`${PAGE}?rule=${HIGH_ID}`);

  const table = await screen.findByRole("table", { name: "Alert rules" });
  const high = within(table).getByRole("row", { name: /^High error rate\b/ });
  act(() => {
    high.focus();
  });

  await user.keyboard("{ArrowDown}");
  expect(router.state.location.search).toMatchObject({ rule: LOW_ID });
  expect(within(table).getByRole("row", { name: /^Low helpfulness\b/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );

  await user.keyboard("{ArrowDown}");
  expect(router.state.location.search).toMatchObject({ rule: DAILY_ID });

  await user.keyboard("{ArrowUp}");
  expect(router.state.location.search).toMatchObject({ rule: LOW_ID });
});

it("does not open the rule when its switch is clicked", async () => {
  server.use(
    http.patch(RULE, () => HttpResponse.json({ ...rulesFixture.data[1], enabled: false })),
  );

  const { router, user } = await renderApp(PAGE);
  const table = await screen.findByRole("table", { name: "Alert rules" });
  await user.click(within(table).getByRole("switch", { name: "Enable High error rate" }));

  expect(router.state.location.search.rule).toBeUndefined();
  expect(screen.queryByRole("region", PANEL)).toBeNull();
});

it("opens the rule in the panel from a recent event", async () => {
  const { router, user } = await renderApp(PAGE);

  const events = await screen.findByRole("table", { name: "Recent alert events" });
  const [link] = within(events).getAllByRole("link", { name: "High error rate" });
  if (link === undefined) {
    throw new Error("no event links to the high error rate rule");
  }
  await user.click(link);

  expect(router.state.location.search).toMatchObject({ rule: HIGH_ID });
  expect(await screen.findByRole("region", PANEL)).toBeInTheDocument();
});

/** Row `index` of a table; index 0 is the header. */
function rowAt(table: HTMLElement, index: number): HTMLElement {
  const row = within(table).getAllByRole("row")[index];
  if (!row) {
    throw new Error(`no row ${index}`);
  }
  return row;
}

const OPEN_HIGH = `${PAGE}?rule=${FIRING_RULE_ID}`;

it("shows the rule and its history in the panel", async () => {
  await renderApp(OPEN_HIGH);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByText("Team Slack, Pager webhook")).toBeInTheDocument();
  expect(within(panel).getByText("environment = 'production'")).toBeInTheDocument();
  expect(within(panel).getByText("Every 60 minutes while firing")).toBeInTheDocument();

  const table = await within(panel).findByRole("table", { name: "Alert events" });
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(3);

  const fired = rowAt(table, 1);
  expect(within(fired).getByText("Fired")).toBeInTheDocument();
  expect(fired).toHaveTextContent("12.5% · 48 samples");
  expect(within(fired).getByText("Pager webhook")).toBeInTheDocument();
  expect(within(fired).getByText("Failed")).toBeInTheDocument();
  expect(within(fired).getByText("the endpoint answered 500")).toBeInTheDocument();

  expect(rowAt(table, 3)).toHaveTextContent("2 attempts");
});

it("shows the last error in the panel", async () => {
  server.use(
    http.get(RULE, () => HttpResponse.json({ ...ruleFixture, last_error: "query timed out" })),
  );

  await renderApp(OPEN_HIGH);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("alert")).toHaveTextContent(
    "The last evaluation failed: query timed out",
  );
});

it("pages the rule's history in the panel", async () => {
  server.use(
    http.get(RULE_EVENTS, ({ request }) => {
      if (new URL(request.url).searchParams.get("cursor") === "c1") {
        return HttpResponse.json({
          data: [
            {
              ...eventsFixture.data[0],
              id: "01900000-0000-7000-8000-000000000529",
              kind: "reminder",
            },
          ],
          next_cursor: null,
        });
      }
      return HttpResponse.json({ ...eventsFixture, next_cursor: "c1" });
    }),
  );

  const { user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  const table = await within(panel).findByRole("table", { name: "Alert events" });
  await user.click(within(panel).getByRole("button", { name: "Load more" }));

  expect(await within(table).findByText("Reminder")).toBeInTheDocument();
});

it("disables the rule from the panel", async () => {
  let body: unknown;
  server.use(
    http.patch(RULE, async ({ request }) => {
      body = await request.json();
      return HttpResponse.json({ ...ruleFixture, enabled: false, state: "ok" });
    }),
  );

  const { router, user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  await user.click(await within(panel).findByRole("switch", { name: "Enabled" }));

  await waitFor(() => expect(body).toEqual({ enabled: false }));
  expect(await within(panel).findByText("Disabled")).toBeInTheDocument();
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("shows the plan limit when enabling from the panel", async () => {
  server.use(
    http.get(RULE, () => HttpResponse.json({ ...ruleFixture, enabled: false })),
    http.patch(RULE, () =>
      problemResponse(403, "plan_limit", {
        detail: "Your plan allows 1 enabled alert rule.",
      }),
    ),
  );

  const { user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  await user.click(await within(panel).findByRole("switch", { name: "Enabled" }));

  expect(
    await within(panel).findByText("Your plan allows 1 enabled alert rule."),
  ).toBeInTheDocument();
  expect(within(panel).getByText("Upgrade your plan or disable another rule.")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("shows other toggle errors next to the panel switch", async () => {
  server.use(http.patch(RULE, () => problemResponse(500, "internal")));

  const { user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  await user.click(await within(panel).findByRole("switch", { name: "Enabled" }));

  expect(await within(panel).findByText("Internal Server Error")).toBeInTheDocument();
});

it("deletes the rule from the panel and closes it", async () => {
  server.use(http.delete(RULE, () => new HttpResponse(null, { status: 204 })));

  const { router, user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  await user.click(await within(panel).findByRole("button", { name: "Delete rule" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete alert rule?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  await waitFor(() => expect(router.state.location.search.rule).toBeUndefined());
  await waitFor(() => expect(screen.queryByRole("region", PANEL)).toBeNull());
  await waitFor(() => expect(router.state.status).toBe("idle"));
});

it("keeps the delete dialog open when the delete fails", async () => {
  server.use(http.delete(RULE, () => problemResponse(500, "internal")));

  const { user } = await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  await user.click(await within(panel).findByRole("button", { name: "Delete rule" }));
  const dialog = await screen.findByRole("alertdialog", { name: "Delete alert rule?" });
  await user.click(within(dialog).getByRole("button", { name: "Delete" }));

  expect(await within(dialog).findByText("Internal Server Error")).toBeInTheDocument();
  expect(screen.getByRole("alertdialog", { name: "Delete alert rule?" })).toBeInTheDocument();
});

it("links the panel's Edit to the editor", async () => {
  await renderApp(OPEN_HIGH);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("link", { name: "Edit" })).toHaveAttribute(
    "href",
    `${PAGE}/${FIRING_RULE_ID}/edit`,
  );
});

it("disables the panel's Edit for viewers", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(OPEN_HIGH);

  const panel = await screen.findByRole("region", PANEL);
  expect(await within(panel).findByRole("button", { name: "Edit" })).toBeDisabled();
});

it("does not let viewers change the rule in the panel", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  const toggle = await within(panel).findByRole("switch", { name: "Enabled" });
  const del = within(panel).getByRole("button", { name: "Delete rule" });

  expect(toggle).toBeDisabled();
  expect(del).toBeDisabled();
  expect(toggle.parentElement).toHaveAccessibleDescription(CANNOT_MANAGE);
  expect(del.parentElement).toHaveAccessibleDescription(CANNOT_MANAGE);
});

it("shows the edge values of the panel's events and channels", async () => {
  server.use(
    http.get(RULE_EVENTS, () =>
      HttpResponse.json({
        next_cursor: null,
        data: [{ ...eventsFixture.data[0], metric: "cost", value: 1.23456, deliveries: [] }],
      }),
    ),
    http.get(RULE, () =>
      HttpResponse.json({ ...ruleFixture, channel_ids: [...ruleFixture.channel_ids, "unknown"] }),
    ),
  );

  await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  const table = await within(panel).findByRole("table", { name: "Alert events" });

  expect(rowAt(table, 1)).toHaveTextContent("1.235");
  expect(within(rowAt(table, 1)).getByText("No channels")).toBeInTheDocument();
  expect(
    await within(panel).findByText("Team Slack, Pager webhook, Deleted channel"),
  ).toBeInTheDocument();
});

it("labels a no data event in the panel, which is neither fired nor resolved", async () => {
  server.use(
    http.get(RULE_EVENTS, () =>
      HttpResponse.json({
        data: [
          { ...eventsFixture.data[0], kind: "no_data", value: null, count: 0, deliveries: [] },
        ],
        next_cursor: null,
      }),
    ),
  );

  await renderApp(OPEN_HIGH);
  const panel = await screen.findByRole("region", PANEL);
  const table = await within(panel).findByRole("table", { name: "Alert events" });

  expect(rowAt(table, 1)).toHaveTextContent("No data");
  expect(table).not.toHaveTextContent("Resolved");
});

it("shows the empty history in the panel", async () => {
  server.use(http.get(RULE_EVENTS, () => HttpResponse.json({ data: [], next_cursor: null })));

  await renderApp(OPEN_HIGH);

  const panel = await screen.findByRole("region", PANEL);
  expect(
    await within(panel).findByText("No events yet. Events appear when the rule fires or resolves."),
  ).toBeInTheDocument();
});
