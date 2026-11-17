import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset } from "@/mocks/generated/dataset";
import { compareUsers, largeHandlers } from "@/mocks/generated/handlers";
import { server } from "@/mocks/node";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;

type User = Omit<Schema<"EndUserSummary">, "activity">;

function user(overrides: Partial<User> & { user_id: string }): User {
  return {
    first_seen: "2026-01-01T00:00:00.000000000Z",
    last_seen: "2026-01-02T00:00:00.000000000Z",
    trace_count: 1,
    session_count: 1,
    error_count: 0,
    error_trace_count: 0,
    total_tokens: 0,
    cost_total: 0,
    scores: [],
    metadata: {},
    ...overrides,
  };
}

function sortedIds(users: User[], sort: string, order: "asc" | "desc"): string[] {
  return [...users]
    .sort((left, right) => compareUsers(left, right, sort, order))
    .map((row) => row.user_id);
}

it("puts null total_tokens last in both directions", () => {
  const users = [
    user({ user_id: "user-a", total_tokens: 5 }),
    user({ user_id: "user-b", total_tokens: undefined }),
    user({ user_id: "user-c", total_tokens: 2 }),
  ];
  expect(sortedIds(users, "total_tokens", "asc")).toEqual(["user-c", "user-a", "user-b"]);
  expect(sortedIds(users, "total_tokens", "desc")).toEqual(["user-a", "user-c", "user-b"]);
});

it("breaks ties by user_id in the sort direction, nulls included", () => {
  const users = [
    user({ user_id: "user-b", total_tokens: 5 }),
    user({ user_id: "user-a", total_tokens: 5 }),
    user({ user_id: "user-y", total_tokens: undefined }),
    user({ user_id: "user-x", total_tokens: undefined }),
  ];
  expect(sortedIds(users, "total_tokens", "asc")).toEqual(["user-a", "user-b", "user-x", "user-y"]);
  expect(sortedIds(users, "total_tokens", "desc")).toEqual([
    "user-b",
    "user-a",
    "user-y",
    "user-x",
  ]);
});

it("puts traces per session last when a user has no sessions", () => {
  const users = [
    user({ user_id: "user-x", trace_count: 4, session_count: 2 }),
    user({ user_id: "user-y", trace_count: 3, session_count: 0 }),
    user({ user_id: "user-z", trace_count: 1, session_count: 1 }),
  ];
  expect(sortedIds(users, "traces_per_session", "asc")).toEqual(["user-z", "user-x", "user-y"]);
  expect(sortedIds(users, "traces_per_session", "desc")).toEqual(["user-x", "user-z", "user-y"]);
});

it("builds the same dataset for a seed, quickly", () => {
  const started = performance.now();
  const first = createDataset({ seed: 1, now: NOW, traces: 5000 });
  const elapsed = performance.now() - started;
  const second = createDataset({ seed: 1, now: NOW, traces: 5000 });

  expect(elapsed).toBeLessThan(1000);
  expect(first.traces[0]?.trace_id).toBe(second.traces[0]?.trace_id);
  expect(first.traces[0]?.span_count).toBe(10_000);
  expect(first.traces[1]).toEqual(second.traces[1]);
});

it("lays the big trace out in pre-order", () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 2 });
  const trace = dataset.traces[0];
  const detail = trace ? dataset.traceDetail(trace.trace_id) : undefined;
  expect(detail?.spans).toHaveLength(10_000);

  const seen = new Map<string, { depth: number; index: number; start: number; end: number }>();
  let previousDepth = -1;
  for (const [index, row] of detail?.spans.entries() ?? []) {
    expect(row.depth).toBeLessThanOrEqual(previousDepth + 1);
    const start = Date.parse(row.span.start_time);
    const end = Date.parse(row.span.end_time);
    expect(end).toBeGreaterThanOrEqual(start);
    if (row.depth === 0 || row.detached) {
      expect(row.span.parent_span_id).toBeUndefined();
    } else {
      const parent = seen.get(row.span.parent_span_id ?? "");
      expect(parent?.depth).toBe(row.depth - 1);
      expect(parent?.index).toBeLessThan(index);
      expect(start).toBeGreaterThanOrEqual(parent?.start ?? 0);
      expect(end).toBeLessThanOrEqual(parent?.end ?? 0);
    }
    seen.set(row.span.span_id, { depth: row.depth, index, start, end });
    previousDepth = row.depth;
  }
  expect(detail?.spans.some((row) => row.detached)).toBe(true);
});

it("pages a list without gaps or duplicates", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));

  const whole = await fetch(`${PROJECT}/traces?limit=150`);
  const all = (await whole.json()) as Schema<"TracePage">;
  const ids: string[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < 3; page += 1) {
    const url = new URL(`${PROJECT}/traces`);
    url.searchParams.set("limit", "50");
    if (cursor) {
      url.searchParams.set("cursor", cursor);
    }
    const response = await fetch(url);
    const body = (await response.json()) as Schema<"TracePage">;
    ids.push(...body.data.map((trace) => trace.trace_id));
    cursor = body.next_cursor;
  }

  expect(ids).toEqual(all.data.map((trace) => trace.trace_id));
  expect(new Set(ids).size).toBe(ids.length);
});

it("sorts traces by duration ascending", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 80 });
  server.use(...largeHandlers(dataset));
  const response = await fetch(`${PROJECT}/traces?sort=duration&order=asc&limit=80`);
  const body = (await response.json()) as Schema<"TracePage">;
  const durations = body.data.map((trace) => trace.duration_ns);
  expect(durations).toEqual([...durations].sort((left, right) => left - right));
});

it("sorts users by error rate descending", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const response = await fetch(`${PROJECT}/users?sort=error_rate&order=desc&limit=1000`);
  const body = (await response.json()) as Schema<"EndUserPage">;
  const rates = body.data.map((user) => user.error_trace_count / user.trace_count);
  expect(rates.length).toBeGreaterThan(1);
  expect(rates).toEqual([...rates].sort((left, right) => right - left));
});

it("hides evaluation traces until include_eval", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const plain = (await (await fetch(`${PROJECT}/traces?limit=1000`)).json()) as Schema<"TracePage">;
  const withEval = (await (
    await fetch(`${PROJECT}/traces?limit=1000&include_eval=true`)
  ).json()) as Schema<"TracePage">;

  expect(plain.data.every((trace) => trace.environment !== "cetra-eval")).toBe(true);
  expect(withEval.data.some((trace) => trace.environment === "cetra-eval")).toBe(true);
  expect(withEval.data.length).toBeGreaterThan(plain.data.length);
});

it("rejects a cursor that does not decode", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 5 })));
  const response = await fetch(`${PROJECT}/traces?cursor=${encodeURIComponent("@@@")}`);
  expect(response.status).toBe(400);
  expect(await response.json()).toMatchObject({ code: "bad_request", status: 400 });
});

it("returns an invalid filter when the text is bad", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 5 })));
  const response = await fetch(`${PROJECT}/traces?filter=bad`);
  expect(response.status).toBe(400);
  expect(await response.json()).toMatchObject({
    code: "invalid_filter",
    errors: [{ field: "filter", message: "unknown column bad", position: { line: 1, column: 1 } }],
  });
});

it("keeps session and user totals equal to their traces", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 200 });
  server.use(...largeHandlers(dataset));
  const traces = dataset.traces.filter(
    (trace) =>
      trace.environment !== "cetra-eval" && trace.metadata["cetra.experiment_id"] === undefined,
  );
  const sessions = (await (
    await fetch(`${PROJECT}/sessions?limit=1000`)
  ).json()) as Schema<"SessionPage">;
  const users = (await (
    await fetch(`${PROJECT}/users?limit=1000`)
  ).json()) as Schema<"EndUserPage">;

  expect(sessions.data.reduce((sum, session) => sum + session.trace_count, 0)).toBe(
    traces.filter((trace) => trace.session_id).length,
  );
  for (const session of sessions.data) {
    const rows = traces.filter((trace) => trace.session_id === session.session_id);
    expect(session.trace_count).toBe(rows.length);
    expect(session.error_count).toBe(rows.reduce((sum, trace) => sum + trace.error_count, 0));
    expect(session.total_tokens).toBe(
      rows.reduce((sum, trace) => sum + (trace.total_tokens ?? 0), 0),
    );
    expect(session.cost_total).toBeCloseTo(
      rows.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0),
    );
  }
  for (const user of users.data) {
    const rows = traces.filter((trace) => trace.user_id === user.user_id);
    expect(user.trace_count).toBe(rows.length);
    expect(user.error_count).toBe(rows.reduce((sum, trace) => sum + trace.error_count, 0));
    expect(user.session_count).toBe(
      new Set(rows.flatMap((trace) => (trace.session_id ? [trace.session_id] : []))).size,
    );
    expect(user.total_tokens).toBe(rows.reduce((sum, trace) => sum + (trace.total_tokens ?? 0), 0));
    expect(user.cost_total).toBeCloseTo(
      rows.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0),
    );
  }
});

it("gives each listed user 24 activity buckets, error traces and user metadata", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 200 });
  server.use(...largeHandlers(dataset));
  const users = (await (
    await fetch(`${PROJECT}/users?limit=1000`)
  ).json()) as Schema<"EndUserPage">;

  expect(users.data.length).toBeGreaterThan(0);
  for (const user of users.data) {
    expect(user.activity).toHaveLength(24);
    expect(user.activity.reduce((sum, count) => sum + count, 0)).toBe(user.trace_count);
    expect(user.error_trace_count).toBeLessThanOrEqual(user.trace_count);
    // Stored values are JSON-encoded strings (spec 01 § metadata), so the quotes are part of the value.
    expect(user.metadata).toBeDefined();
    expect(['"free"', '"pro"', '"team"']).toContain(user.metadata?.plan);
  }
});

it("leaves the project fixture in place", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 2 })));
  const response = await fetch(PROJECT);
  expect(response.status).toBe(200);
  expect(await response.json()).toMatchObject({ slug: "support-bot", name: "Support bot" });
});

it("answers an unknown span with not_found", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 2 })));
  const response = await fetch(
    `${PROJECT}/traces/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/spans/0000000000000001`,
  );
  expect(response.status).toBe(404);
  expect(await response.json()).toMatchObject({ code: "not_found" });
});

it("defines the generated scores so lists can colour them", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 5 })));
  const response = await fetch(`${PROJECT}/score-definitions`);
  const body = (await response.json()) as { data: { name: string; direction: string }[] };
  expect(body.data.map((item) => [item.name, item.direction])).toEqual([
    ["correctness", "higher"],
    ["relevance", "higher"],
  ]);
});
