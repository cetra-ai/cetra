import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset, visibleTraces } from "@/mocks/generated/dataset";
import {
  compareUsers,
  largeHandlers,
  stepsOfFilter,
  userIdOfFilter,
} from "@/mocks/generated/handlers";
import { server } from "@/mocks/node";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const DAY_MS = 86_400_000;

/** `POST …/facets` with a batch body (D-C-24). */
function postFacets(body: unknown): Promise<Response> {
  return fetch(`${PROJECT}/facets`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

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
    ["exact", "higher"],
    ["quality", "higher"],
    ["tone", "none"],
    ["tests_pass", "higher"],
    ["verdict", "none"],
    ["resolved", "higher"],
    ["schema_valid", "higher"],
    ["fields_correct", "none"],
  ]);
});

it("serves the story queues' score definitions after the generated ones", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 5 })));
  const response = await fetch(`${PROJECT}/score-definitions`);
  const body = (await response.json()) as { data: { id: string }[] };
  const ids = body.data.map((item) => item.id);
  const generated = [
    "01900000-0000-7000-8000-000000000611",
    "01900000-0000-7000-8000-000000000612",
    "01900000-0000-7000-8000-000000000615",
  ];
  const story = [
    "01900000-0000-7000-8000-000000570601",
    "01900000-0000-7000-8000-000000570602",
    "01900000-0000-7000-8000-000000570603",
    "01900000-0000-7000-8000-000000570605",
    "01900000-0000-7000-8000-000000570606",
  ];
  const at = (id: string) => ids.indexOf(id);
  for (const id of generated) {
    expect(at(id)).toBeGreaterThanOrEqual(0);
  }
  expect(at(generated[2] ?? "")).toBeLessThan(at(story[0] ?? ""));
  expect(ids.slice(-5)).toEqual(story);
});

it("keeps the fixture queue's review as it was", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 5 });
  server.use(...largeHandlers(dataset));
  const review = async (id: string) =>
    (
      (await (await fetch(`${PROJECT}/annotation-queues/${id}`)).json()) as {
        definition_ids: string[];
      }
    ).definition_ids;

  const correctness = "01900000-0000-7000-8000-000000000611";
  const relevance = "01900000-0000-7000-8000-000000000612";
  expect(await review("01900000-0000-7000-8000-000000000a01")).toEqual([correctness, relevance]);
  expect(await review("01900000-0000-7000-8000-000000570501")).toEqual([correctness, relevance]);
  expect(await review("01900000-0000-7000-8000-000000570502")).toEqual([
    "01900000-0000-7000-8000-000000570601",
    "01900000-0000-7000-8000-000000570602",
  ]);
  expect(await review("01900000-0000-7000-8000-000000570503")).toEqual([
    "01900000-0000-7000-8000-000000570603",
    "01900000-0000-7000-8000-000000000615",
  ]);

  const storyItem = (await (
    await fetch(
      `${PROJECT}/annotation-queues/01900000-0000-7000-8000-000000570502/items/01900000-0000-7000-8000-000000570512`,
    )
  ).json()) as { trace_id: string };
  expect(storyItem.trace_id).toBe(dataset.traces[4]?.trace_id);
  const fixtureItem = (await (
    await fetch(
      `${PROJECT}/annotation-queues/01900000-0000-7000-8000-000000000a01/items/01900000-0000-7000-8000-000000000a11`,
    )
  ).json()) as { trace_id: string };
  expect(fixtureItem.trace_id).toBe(dataset.traces[0]?.trace_id);
});

/** A trace the default lists show: no evaluation and no experiment trace. */
function isVisible(trace: Schema<"TraceSummary">): boolean {
  return trace.environment !== "cetra-eval" && trace.metadata["cetra.experiment_id"] === undefined;
}

/** The visible trace of the user with the most visible traces. */
function busiestUser(dataset: Dataset): string {
  const counts = new Map<string, number>();
  for (const trace of dataset.traces) {
    if (trace.user_id && isVisible(trace)) {
      counts.set(trace.user_id, (counts.get(trace.user_id) ?? 0) + 1);
    }
  }
  const [userId] = [...counts.entries()].sort((left, right) => right[1] - left[1])[0] ?? [];
  if (!userId) {
    throw new Error("the dataset has a user with visible traces");
  }
  return userId;
}

it("reads the user id of a user_id filter only when it has exactly that form", () => {
  expect(userIdOfFilter("user_id = 'o''brien'")).toBe("o'brien");
  expect(userIdOfFilter("  user_id='user-007' ")).toBe("user-007");
  expect(userIdOfFilter("user_id = 'a' and name = 'b'")).toBeUndefined();
  expect(userIdOfFilter("name = 'user-007'")).toBeUndefined();
  expect(userIdOfFilter(undefined)).toBeUndefined();
});

it("stepsOfFilter reads the trace ids and kinds", () => {
  const steps = stepsOfFilter(
    "trace_id IN ('a1000000000000000000000000000001', 'a1000000000000000000000000000002') AND span_kind IN ('llm', 'tool', 'retriever', 'guardrail')",
  );
  expect(steps?.traceIds).toEqual(
    new Set(["a1000000000000000000000000000001", "a1000000000000000000000000000002"]),
  );
  expect(steps?.kinds).toEqual(new Set(["llm", "tool", "retriever", "guardrail"]));
});

it("stepsOfFilter refuses other filters", () => {
  expect(stepsOfFilter("user_id = 'x'")).toBeUndefined();
  expect(stepsOfFilter("trace_id IN ('a1000000000000000000000000000001')")).toBeUndefined();
  expect(
    stepsOfFilter("trace_id IN (a1000000000000000000000000000001) AND span_kind IN ('llm')"),
  ).toBeUndefined();
  expect(stepsOfFilter("trace_id IN () AND span_kind IN ('llm')")).toBeUndefined();
  expect(stepsOfFilter(undefined)).toBeUndefined();
});

it("lists only one user's traces for a user_id filter, and flags other filters as ignored", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const userId = busiestUser(dataset);
  const expected = dataset.traces
    .filter((trace) => trace.user_id === userId && isVisible(trace))
    .map((trace) => trace.trace_id)
    .sort();

  const url = new URL(`${PROJECT}/traces`);
  url.searchParams.set("filter", `user_id = '${userId}'`);
  url.searchParams.set("limit", "1000");
  const response = await fetch(url);
  const body = (await response.json()) as Schema<"TracePage">;
  expect(response.headers.get("X-Mock-Filter-Ignored")).toBeNull();
  expect(body.data.map((trace) => trace.trace_id).sort()).toEqual(expected);

  const other = new URL(`${PROJECT}/traces`);
  other.searchParams.set("filter", "metadata['plan'] = 'pro'");
  const flagged = await fetch(other);
  expect(flagged.headers.get("X-Mock-Filter-Ignored")).toBe("1");
});

it("still ignores a filter on the spans list other than user_id", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const url = new URL(`${PROJECT}/spans`);
  url.searchParams.set("filter", "status = 'error'");
  const response = await fetch(url);
  expect(response.status).toBe(200);
  expect(response.headers.get("X-Mock-Filter-Ignored")).toBe("1");
});

it("applies a status filter to the trace list", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const expected = dataset.traces
    .filter((trace) => isVisible(trace) && trace.status === "error")
    .map((trace) => trace.trace_id)
    .sort();

  const url = new URL(`${PROJECT}/traces`);
  url.searchParams.set("filter", "status = 'error'");
  url.searchParams.set("limit", "1000");
  const response = await fetch(url);
  const body = (await response.json()) as Schema<"TracePage">;
  expect(response.headers.get("X-Mock-Filter-Ignored")).toBeNull();
  expect(body.data.map((trace) => trace.trace_id).sort()).toEqual(expected);
});

it("applies a tags filter combined with a name filter", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));

  const url = new URL(`${PROJECT}/traces`);
  url.searchParams.set("filter", "tags @> ['retry'] and name = 'coding-agent'");
  url.searchParams.set("limit", "1000");
  const response = await fetch(url);
  const body = (await response.json()) as Schema<"TracePage">;
  expect(response.headers.get("X-Mock-Filter-Ignored")).toBeNull();
  expect(body.data.map((trace) => trace.trace_id)).toEqual([dataset.traces[4]?.trace_id]);
});

it("sorts a filtered list", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));

  const url = new URL(`${PROJECT}/traces`);
  url.searchParams.set("filter", "tags @> ['voice']");
  url.searchParams.set("sort", "duration");
  url.searchParams.set("order", "desc");
  url.searchParams.set("limit", "1000");
  const response = await fetch(url);
  const body = (await response.json()) as Schema<"TracePage">;
  const durations = body.data.map((trace) => trace.duration_ns);
  expect(durations.length).toBeGreaterThan(0);
  expect(durations).toEqual([...durations].sort((left, right) => right - left));
  for (const trace of body.data) {
    expect(trace.tags).toContain("voice");
  }
});

it("lists only the spans of one user's traces for a user_id filter, without the ignored flag", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const userId = busiestUser(dataset);
  const expected = dataset.traces
    .filter((trace) => trace.user_id === userId && isVisible(trace))
    .flatMap((trace) => dataset.traceDetail(trace.trace_id)?.spans ?? [])
    .map((row) => row.span.span_id)
    .sort();

  /** Every page of a spans list for the user's filter, and whether any page was flagged. */
  async function allSpans(sort: string | undefined) {
    const ids: string[] = [];
    let flagged = false;
    let cursor: string | null = null;
    do {
      const url = new URL(`${PROJECT}/spans`);
      url.searchParams.set("filter", `user_id = '${userId}'`);
      url.searchParams.set("limit", "1000");
      if (sort) {
        url.searchParams.set("sort", sort);
      }
      if (cursor) {
        url.searchParams.set("cursor", cursor);
      }
      const response = await fetch(url);
      flagged ||= response.headers.get("X-Mock-Filter-Ignored") !== null;
      const body = (await response.json()) as Schema<"SpanPage">;
      ids.push(...body.data.map((span) => span.span_id));
      cursor = body.next_cursor ?? null;
    } while (cursor);
    return { ids: ids.sort(), flagged };
  }

  const byStart = await allSpans(undefined);
  expect(byStart.flagged).toBe(false);
  expect(byStart.ids).toEqual(expected);

  const byDuration = await allSpans("duration");
  expect(byDuration.flagged).toBe(false);
  expect(byDuration.ids).toEqual(expected);
});

it("the span list keeps only the listed traces' steps for the steps filter", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const picked = dataset.traces
    .filter(
      (trace) =>
        isVisible(trace) &&
        (dataset.traceDetail(trace.trace_id)?.spans ?? []).some(
          (row) => row.span.span_kind === "llm" || row.span.span_kind === "tool",
        ),
    )
    .slice(0, 2);
  if (picked.length < 2) {
    throw new Error("the dataset has two visible traces with llm or tool spans");
  }
  const ids = picked.map((trace) => trace.trace_id);
  const filter = `trace_id IN (${ids.map((id) => `'${id}'`).join(", ")}) AND span_kind IN ('llm', 'tool')`;
  const expected = picked
    .flatMap((trace) => dataset.traceDetail(trace.trace_id)?.spans ?? [])
    .map((row) => row.span)
    .filter((span) => span.span_kind === "llm" || span.span_kind === "tool")
    .map((span) => span.span_id)
    .sort();

  /** Every page of a spans list for the steps filter, and whether any page was flagged. */
  async function allSteps(sort: string | undefined) {
    const spans: Schema<"SpanSummary">[] = [];
    let flagged = false;
    let cursor: string | null = null;
    do {
      const url = new URL(`${PROJECT}/spans`);
      url.searchParams.set("filter", filter);
      url.searchParams.set("limit", "1000");
      if (sort) {
        url.searchParams.set("sort", sort);
      }
      if (cursor) {
        url.searchParams.set("cursor", cursor);
      }
      const response = await fetch(url);
      flagged ||= response.headers.get("X-Mock-Filter-Ignored") !== null;
      const body = (await response.json()) as Schema<"SpanPage">;
      spans.push(...body.data);
      cursor = body.next_cursor ?? null;
    } while (cursor);
    return { spans, flagged };
  }

  for (const sort of [undefined, "duration"]) {
    const { spans, flagged } = await allSteps(sort);
    expect(flagged, String(sort)).toBe(false);
    expect(
      spans.some((span) => !ids.includes(span.trace_id)),
      String(sort),
    ).toBe(false);
    expect(
      spans.every((span) => span.span_kind === "llm" || span.span_kind === "tool"),
      String(sort),
    ).toBe(true);
    expect(spans.map((span) => span.span_id).sort(), String(sort)).toEqual(expected);
  }
});

it("honours from and to on the user detail", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  const userId = busiestUser(dataset);
  const mine = dataset.traces.filter((trace) => trace.user_id === userId && isVisible(trace));
  const latest = Math.max(...mine.map((trace) => Date.parse(trace.start_time)));
  const day = Math.floor(latest / DAY_MS) * DAY_MS;
  const inDay = mine.filter((trace) => {
    const start = Date.parse(trace.start_time);
    return start >= day && start < day + DAY_MS;
  });

  const response = await fetch(`${PROJECT}/users/${userId}?from=${day}&to=${day + DAY_MS}`);
  const body = (await response.json()) as Schema<"EndUserDetail">;
  expect(response.status).toBe(200);
  expect(body.user.trace_count).toBe(inDay.length);

  const outside = await fetch(`${PROJECT}/users/${userId}?from=946684800000&to=946771200000`);
  expect(outside.status).toBe(404);
});

it("counts only the traces in the range on the users list, as the user detail does", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 200 });
  server.use(...largeHandlers(dataset));
  const window = `from=${new Date(NOW - 2 * DAY_MS).toISOString()}&to=${new Date(NOW).toISOString()}`;
  const users = (await (
    await fetch(`${PROJECT}/users?limit=1000&${window}`)
  ).json()) as Schema<"EndUserPage">;
  expect(users.data.length).toBeGreaterThan(0);
  for (const row of users.data) {
    const response = await fetch(`${PROJECT}/users/${row.user_id}?${window}`);
    expect(response.status).toBe(200);
    const detail = (await response.json()) as Schema<"EndUserDetail">;
    expect(detail.user.trace_count).toBe(row.trace_count);
    expect(detail.user.session_count).toBe(row.session_count);
  }
});

it("counts a values facet over the rows the list shows", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));
  const body = (await (
    await postFacets({ view: "traces", fields: [{ field: "status", limit: 50 }] })
  ).json()) as Schema<"FacetsResponse">;
  const facet = body.results[0]?.facet;

  const visible = visibleTraces(dataset.traces, false, false);
  const expected = new Map<string, number>();
  for (const trace of visible) {
    expected.set(trace.status, (expected.get(trace.status) ?? 0) + 1);
  }
  expect(facet?.kind).toBe("values");
  expect(facet?.total).toBe(visible.length);
  expect(facet?.missing).toBe(0);
  expect(facet?.more).toBe(false);
  expect(new Map((facet?.values ?? []).map((row) => [row.value, row.count]))).toEqual(expected);
});

it("answers every field of a batch in order", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));
  const body = (await (
    await postFacets({
      view: "traces",
      fields: [{ field: "status" }, { field: "duration_ms" }],
    })
  ).json()) as Schema<"FacetsResponse">;
  expect(body.view).toBe("traces");
  expect(body.results.map((result) => result.field)).toEqual(["status", "duration_ms"]);
  expect(body.results[0]?.facet?.kind).toBe("values");
  expect(body.results[1]?.facet?.kind).toBe("histogram");
});

it("counts a list field once per distinct element, and facets metadata keys", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));
  const visible = visibleTraces(dataset.traces, false, false);

  const tags = (
    (await (
      await postFacets({ view: "traces", fields: [{ field: "tags", limit: 100 }] })
    ).json()) as Schema<"FacetsResponse">
  ).results[0]?.facet;
  const tagCounts = new Map<string, number>();
  let untagged = 0;
  for (const trace of visible) {
    if (trace.tags.length === 0) {
      untagged += 1;
    }
    for (const tag of new Set(trace.tags)) {
      tagCounts.set(tag, (tagCounts.get(tag) ?? 0) + 1);
    }
  }
  expect(tags?.missing).toBe(untagged);
  expect(new Map((tags?.values ?? []).map((row) => [row.value, row.count]))).toEqual(tagCounts);

  const keys = (
    (await (
      await postFacets({ view: "traces", fields: [{ field: "metadata", limit: 100 }] })
    ).json()) as Schema<"FacetsResponse">
  ).results[0]?.facet;
  expect(keys?.kind).toBe("keys");
  const keyNames = new Set<string>();
  for (const trace of visible) {
    for (const key of Object.keys(trace.metadata)) {
      if (!key.startsWith("cetra.")) {
        keyNames.add(key);
      }
    }
  }
  expect(new Set((keys?.values ?? []).map((row) => row.value))).toEqual(keyNames);
  expect((keys?.values ?? []).some((row) => row.value.startsWith("cetra."))).toBe(false);
});

it("answers a histogram whose buckets cover every row with a value", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));
  const body = (await (
    await postFacets({ view: "traces", fields: [{ field: "duration_ms" }] })
  ).json()) as Schema<"FacetsResponse">;
  const facet = body.results[0]?.facet;

  const durations = visibleTraces(dataset.traces, false, false).map(
    (trace) => trace.duration_ns / 1e6,
  );
  expect(facet?.kind).toBe("histogram");
  expect(facet?.total).toBe(durations.length);
  expect(facet?.values).toBeUndefined();
  const histogram = facet?.histogram;
  expect(histogram?.min).toBe(Math.min(...durations));
  expect(histogram?.max).toBe(Math.max(...durations));
  const buckets = histogram?.buckets ?? [];
  expect([1, 20, 21]).toContain(buckets.length);
  expect(buckets.reduce((sum, bucket) => sum + bucket.count, 0)).toBe(durations.length);
  for (const [index, bucket] of buckets.entries()) {
    expect(bucket.hi).toBeGreaterThanOrEqual(bucket.lo);
    if (index > 0) {
      expect(bucket.lo).toBeGreaterThanOrEqual(buckets[index - 1]?.lo ?? 0);
    }
  }
});

it("narrows values by q, case-insensitively, and answers the limit", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 300 });
  server.use(...largeHandlers(dataset));
  const first = visibleTraces(dataset.traces, false, false)[0];
  const needle = (first?.name ?? "").slice(0, 3);
  const body = (await (
    await postFacets({
      view: "traces",
      fields: [{ field: "name", q: needle.toUpperCase(), limit: 1 }],
    })
  ).json()) as Schema<"FacetsResponse">;
  const facet = body.results[0]?.facet;
  expect(facet?.values?.length).toBe(1);
  expect(facet?.values?.[0]?.value.toLowerCase()).toContain(needle.toLowerCase());
  expect(typeof facet?.more).toBe("boolean");
});

it("answers a bad field, limit or q as that field's error, and a bad view or search on sessions as 400", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 5 });
  server.use(...largeHandlers(dataset));
  for (const field of [
    { field: "input" },
    { field: "metadata[" },
    { field: "trace_id" },
    { field: "status", limit: 101 },
    { field: "status", q: "   " },
  ]) {
    const body = (await (
      await postFacets({ view: "traces", fields: [{ field: "status" }, field] })
    ).json()) as Schema<"FacetsResponse">;
    expect(body.results[0]?.facet, field.field).toBeDefined();
    expect(body.results[1]?.facet, field.field).toBeUndefined();
    expect(body.results[1]?.error?.code, field.field).toBe("bad_request");
  }
  const badView = await postFacets({ view: "users", fields: [{ field: "status" }] });
  expect(badView.status).toBe(400);
  const sessionsSearch = await postFacets({
    view: "sessions",
    search: "x",
    fields: [{ field: "user_id" }],
  });
  expect(sessionsSearch.status).toBe(400);
});

it("totals the traces the list matches", async () => {
  const dataset = createDataset({ seed: 1, now: NOW, traces: 400 });
  server.use(...largeHandlers(dataset));
  for (const query of ["", "include_eval=true"]) {
    const list = (await (
      await fetch(`${PROJECT}/traces?limit=1000${query ? `&${query}` : ""}`)
    ).json()) as { data: Schema<"TraceSummary">[] };
    const totals = (await (
      await fetch(`${PROJECT}/traces/totals${query ? `?${query}` : ""}`)
    ).json()) as Schema<"TraceTotals">;
    const cost = list.data.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0);
    expect(totals.trace_count, query).toBe(list.data.length);
    expect(totals.error_trace_count, query).toBe(
      list.data.filter((trace) => trace.status === "error").length,
    );
    expect(Math.abs(totals.cost_total - cost), query).toBeLessThan(1e-9);
  }
});
