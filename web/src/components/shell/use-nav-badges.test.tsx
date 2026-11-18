import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { expect, it } from "vitest";
import { $api } from "@/api/client";
import { alertRulesQuery } from "@/api/queries-alerts";
import { experimentsInit } from "@/api/queries-experiments";
import { onlineRulesInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { firingRules, useNavBadges } from "./use-nav-badges";

const ORG = "acme";
const PROJECT = "support-bot";

const ALERT_RULES = "*/api/v1/projects/:project/alert-rules";
const ONLINE_RULES = "*/api/v1/projects/:project/online-rules";
const EXPERIMENTS = "*/api/v1/projects/:project/experiments";

function setup() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity, staleTime: Infinity } },
  });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
  const view = renderHook(() => useNavBadges(ORG, PROJECT), { wrapper });
  return { ...view, queryClient };
}

/** Waits until the three badge queries have settled (success or error). */
async function settled(queryClient: QueryClient) {
  const keys = [
    alertRulesQuery(ORG, PROJECT).queryKey,
    $api.queryOptions(
      "get",
      "/api/v1/projects/{project}/online-rules",
      onlineRulesInit(ORG, PROJECT),
    ).queryKey,
    $api.queryOptions(
      "get",
      "/api/v1/projects/{project}/experiments",
      experimentsInit(ORG, PROJECT),
    ).queryKey,
  ];
  await waitFor(() => {
    for (const key of keys) {
      expect(queryClient.getQueryState(key)?.status).not.toBe("pending");
    }
  });
}

it("counts the firing enabled alert rules, with the page's own query", async () => {
  const { result, queryClient } = setup();
  await settled(queryClient);
  // The fixture has one enabled rule that is firing.
  expect(result.current["/$org/$project/alerts"]).toEqual({
    kind: "count",
    value: 1,
    tone: "destructive",
    label: "1 alert rule firing",
  });
});

it("gives no alert badge when no enabled rule is firing", async () => {
  server.use(
    http.get(ALERT_RULES, () =>
      HttpResponse.json({
        data: [
          {
            id: "01900000-0000-7000-8000-000000000512",
            name: "Daily cost",
            enabled: false,
            condition: { metric: "cost", window: "24h", comparator: "gt", threshold: 50 },
            channel_ids: [],
            cooldown_s: 3600,
            notify_while_firing: false,
            state: "firing",
            created_at: "2026-01-01T09:00:00.000000000Z",
            updated_at: "2026-01-01T09:00:00.000000000Z",
          },
          {
            id: "01900000-0000-7000-8000-000000000511",
            name: "Low helpfulness",
            enabled: true,
            condition: { metric: "score", window: "1h", comparator: "lt", threshold: 0.7 },
            channel_ids: [],
            cooldown_s: 3600,
            notify_while_firing: false,
            state: "ok",
            created_at: "2026-01-01T09:00:00.000000000Z",
            updated_at: "2026-01-01T09:00:00.000000000Z",
          },
        ],
        next_cursor: null,
      }),
    ),
  );
  const { result, queryClient } = setup();
  await settled(queryClient);
  expect(result.current["/$org/$project/alerts"]).toBeUndefined();
});

it("shows a warning dot when a rule is paused on errors", async () => {
  const { result, queryClient } = setup();
  await settled(queryClient);
  // The fixture has one enabled rule paused on errors.
  expect(result.current["/$org/$project/online-rules"]).toEqual({
    kind: "dot",
    tone: "warning",
    label: "An online rule is paused on errors",
  });
});

it("gives no online rules badge for a rule paused on its budget", async () => {
  server.use(
    http.get(ONLINE_RULES, () =>
      HttpResponse.json({
        data: [
          {
            id: "01900000-0000-7000-8000-000000000700",
            name: "Relevance on answers",
            evaluator_id: "01900000-0000-7000-8000-000000000500",
            target: "trace",
            variable_mapping: { input: "trace.input", output: "trace.output" },
            sample_rate: 0.25,
            budget_usd: 5,
            budget_period: "day",
            enabled: true,
            paused_reason: "budget",
            paused_at: "2026-01-01T09:30:00.000000000Z",
            jobs_last_24h: { queued: 0, running: 0, done: 4, failed: 0, skipped: 0 },
            dropped_last_hour: 0,
            created_by: "01900000-0000-7000-8000-000000000100",
            created_at: "2026-01-01T09:00:00.000000000Z",
            updated_at: "2026-01-01T09:30:00.000000000Z",
          },
        ],
        next_cursor: null,
        org_enabled_rules: 1,
      }),
    ),
  );
  const { result, queryClient } = setup();
  await settled(queryClient);
  expect(result.current["/$org/$project/online-rules"]).toBeUndefined();
});

it("counts the running experiments in the loaded pages", async () => {
  server.use(
    http.get(EXPERIMENTS, () =>
      HttpResponse.json({
        data: [
          {
            id: "01900000-0000-7000-8000-000000000e03",
            name: "candidate-2",
            dataset_id: "01900000-0000-7000-8000-000000000c01",
            dataset_name: "faq",
            dataset_version: 2,
            metadata: {},
            repetitions: 1,
            item_count: 3,
            result_count: 1,
            status: "running",
            created_at: "2026-01-01T12:00:00.000000000Z",
          },
          {
            id: "01900000-0000-7000-8000-000000000e04",
            name: "candidate-3",
            dataset_id: "01900000-0000-7000-8000-000000000c01",
            dataset_name: "faq",
            dataset_version: 2,
            metadata: {},
            repetitions: 1,
            item_count: 3,
            result_count: 3,
            status: "running",
            created_at: "2026-01-01T12:30:00.000000000Z",
          },
          {
            id: "01900000-0000-7000-8000-000000000e01",
            name: "baseline",
            dataset_id: "01900000-0000-7000-8000-000000000c01",
            dataset_name: "faq",
            dataset_version: 2,
            metadata: {},
            repetitions: 1,
            item_count: 3,
            result_count: 3,
            status: "completed",
            created_at: "2026-01-01T10:00:00.000000000Z",
            finished_at: "2026-01-01T10:05:00.000000000Z",
          },
        ],
        next_cursor: null,
      }),
    ),
  );
  const { result, queryClient } = setup();
  await settled(queryClient);
  expect(result.current["/$org/$project/experiments"]).toEqual({
    kind: "count",
    value: 2,
    tone: "muted",
    label: "2 experiments running",
  });
});

it("gives no badge from a query that fails", async () => {
  const failure = () => HttpResponse.json({ title: "Server error", status: 500 }, { status: 500 });
  server.use(
    http.get(ALERT_RULES, failure),
    http.get(ONLINE_RULES, failure),
    http.get(EXPERIMENTS, failure),
  );
  const { result, queryClient } = setup();
  await settled(queryClient);
  expect(result.current).toEqual({});
});

it("shares the page's cache entries with the badges", async () => {
  const { queryClient } = setup();
  await settled(queryClient);

  // The alerts page reads this plain entry.
  expect(queryClient.getQueryData(alertRulesQuery(ORG, PROJECT).queryKey)).toBeDefined();

  // The online rules page reads the infinite entry under this key, so it holds the pages.
  const onlineRulesKey = $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(ORG, PROJECT),
  ).queryKey;
  expect(queryClient.getQueryData(onlineRulesKey)).toEqual({
    pages: [expect.objectContaining({ data: expect.any(Array) })],
    pageParams: [null],
  });

  // The experiments page reads the infinite entry for no dataset filter.
  const experimentsKey = $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/experiments",
    experimentsInit(ORG, PROJECT),
  ).queryKey;
  expect(queryClient.getQueryData(experimentsKey)).toEqual({
    pages: [expect.objectContaining({ data: expect.any(Array) })],
    pageParams: [null],
  });
});

function rule(
  id: string,
  name: string,
  enabled: boolean,
  state: Schema<"AlertRule">["state"],
): Schema<"AlertRule"> {
  return {
    id,
    name,
    enabled,
    condition: { metric: "cost", window: "24h", comparator: "gt", threshold: 50 },
    channel_ids: [],
    cooldown_s: 3600,
    notify_while_firing: false,
    state,
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  };
}

it("firingRules keeps enabled firing rules in order", () => {
  const rules = [
    rule("a", "First", true, "firing"),
    rule("b", "Disabled", false, "firing"),
    rule("c", "Calm", true, "ok"),
    rule("d", "Second", true, "firing"),
  ];
  expect(firingRules(rules).map((r) => r.id)).toEqual(["a", "d"]);
});
