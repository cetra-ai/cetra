import { diag, SpanStatusCode } from "@opentelemetry/api";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, Client } from "./client.js";
import { type CetraOptions, ConfigError } from "./config.js";
import { withContext } from "./context.js";
import { Dataset } from "./datasets.js";
import {
  ExperimentResult,
  evaluate,
  type Run,
  type Score,
  type ScorerArgs,
} from "./experiments.js";
import { BASE, FakeRest, type RecordedRequest, type Reply } from "./rest.test-helpers.js";
import { flush, resetForTests } from "./runtime.js";
import { score } from "./scores.js";
import { withSpan } from "./span.js";
import { finished, startMemory } from "./testing.test-helpers.js";
import type { FetchLike } from "./web.js";

/**
 * A runtime whose score queue answers 200 and records its batches in `calls`.
 * Without it the queue would post to the real network and `flush()` would wait
 * through the retries.
 */
function startScoring(options?: Partial<CetraOptions>, calls: ScoreCall[] = []) {
  return startMemory(options, {}, { fetch: scoreFetch(calls) });
}

beforeEach(async () => {
  await resetForTests();
});

afterEach(async () => {
  await resetForTests();
  vi.restoreAllMocks();
});

interface ScoreCall {
  url: string;
  body: string;
}

/** The score queue's transport: records every batch and answers `status`. */
function scoreFetch(calls: ScoreCall[], status = 200): FetchLike {
  return (url, init) => {
    calls.push({ url, body: String(init.body) });
    return Promise.resolve({
      status,
      ok: status >= 200 && status < 300,
      headers: { get: () => null },
    });
  };
}

/** Every score queued so far, flattened from the recorded batches. */
function scoresOf(calls: ScoreCall[]): Record<string, unknown>[] {
  return calls.flatMap(
    (call) => (JSON.parse(call.body) as { scores: Record<string, unknown>[] }).scores,
  );
}

interface Api {
  rest: FakeRest;
  client: Client;
  requests: () => RecordedRequest[];
}

/**
 * A fake cetra API for experiments. `items` are the pinned version's items,
 * `pageSize` the create answer's page. `override` answers first when it returns
 * a reply; the defaults serve the happy path.
 */
function api(
  itemCount: number,
  options: {
    pageSize?: number;
    override?: (request: RecordedRequest, path: string) => Reply | undefined;
  } = {},
): Api {
  const pageSize = options.pageSize ?? 1000;
  const items = Array.from({ length: itemCount }, (_, index) => ({
    id: `item-${String(index + 1)}`,
    input: { n: index + 1 },
    expected_output: index + 1,
    metadata: {},
    version: 2,
  }));
  const rest = new FakeRest((request) => {
    const path = request.url.slice(BASE.length).split("?")[0] ?? "";
    const query = request.url.includes("?") ? (request.url.split("?")[1] ?? "") : "";
    const overridden = options.override?.(request, path);
    if (overridden !== undefined) {
      return overridden;
    }
    if (request.method === "POST" && path === "/experiments") {
      const body = JSON.parse(request.body ?? "{}") as { name: string };
      return {
        status: 201,
        json: {
          experiment: { id: "e1", name: body.name, dataset_id: "d1", dataset_version: 2 },
          items: items.slice(0, pageSize),
          next_cursor: items.length > pageSize ? "c1" : null,
        },
      };
    }
    if (request.method === "GET" && path === "/datasets/d1/items") {
      const tail = items.slice(pageSize);
      return query.includes("cursor=c1")
        ? { status: 200, json: { data: tail, next_cursor: null } }
        : { status: 200, json: { data: [], next_cursor: null } };
    }
    if (request.method === "GET" && path === "/datasets/faq") {
      return {
        status: 200,
        json: { id: "d1", name: "faq", current_version: 2, item_count: items.length },
      };
    }
    if (request.method === "POST" && path === "/experiments/e1/items") {
      const body = JSON.parse(request.body ?? "{}") as { results: unknown[] };
      return { status: 200, json: { stored: body.results.length, skipped: 0 } };
    }
    if (request.method === "POST" && path === "/experiments/e1/finish") {
      return { status: 200, json: { id: "e1", status: "completed" } };
    }
    return { status: 404, json: { code: "not_found" } };
  });
  const client = new Client({
    apiKey: "sk-cetra-test",
    baseUrl: "http://cetra.test",
    fetch: rest.fetch,
    sleep: rest.sleep,
  });
  return { rest, client, requests: () => rest.requests };
}

/** The bodies of the recorded requests whose path ends with `suffix`. */
function bodiesTo(requests: RecordedRequest[], method: string, suffix: string): unknown[] {
  return requests
    .filter((request) => request.method === method && request.url.endsWith(suffix))
    .map((request) => JSON.parse(request.body ?? "{}") as unknown);
}

function resultRows(requests: RecordedRequest[]): Record<string, unknown>[] {
  return bodiesTo(requests, "POST", "/experiments/e1/items").flatMap(
    (body) => (body as { results: Record<string, unknown>[] }).results,
  );
}

describe("evaluate", () => {
  it("needs cetra.init() first, and makes no request", async () => {
    const { client, requests } = api(1);
    await expect(evaluate({ dataset: "faq", task: () => 1, client })).rejects.toBeInstanceOf(
      ConfigError,
    );
    expect(requests()).toHaveLength(0);
  });

  it.each([
    ["concurrency 0", { concurrency: 0 }],
    ["concurrency 65", { concurrency: 65 }],
    ["repetitions 21", { repetitions: 21 }],
    ["an empty experiment name", { experimentName: "" }],
    [
      "two scorers with one name",
      {
        scorers: [
          function dup() {
            return true;
          },
          function dup() {
            return false;
          },
        ],
      },
    ],
    ["an unnamed scorer", { scorers: [(() => true) as unknown as (args: ScorerArgs) => boolean] }],
  ])("refuses %s before any request", async (_label, extra) => {
    startScoring({ baseUrl: "http://127.0.0.1:9" });
    const { client, requests } = api(1);
    await expect(evaluate({ dataset: "faq", task: () => 1, client, ...extra })).rejects.toThrow();
    expect(requests()).toHaveLength(0);
  });

  it("creates the experiment with the dataset, metadata and repetitions", async () => {
    startScoring();
    const { client, requests } = api(2);
    await evaluate({
      dataset: "faq",
      task: () => 1,
      client,
      experimentName: "run-1",
      metadata: { model: "m" },
      repetitions: 2,
    });
    const [create] = bodiesTo(requests(), "POST", "/experiments");
    expect(create).toEqual({
      name: "run-1",
      dataset: "faq",
      repetitions: 2,
      metadata: { model: "m" },
    });
  });

  it("reads the dataset name for the default experiment name", async () => {
    startScoring();
    const { client, requests } = api(1);
    await evaluate({ dataset: "faq", task: () => 1, client });
    const [create] = bodiesTo(requests(), "POST", "/experiments") as { name: string }[];
    expect(create?.name).toMatch(/^faq \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/);
  });

  it("pages the items at the pinned version", async () => {
    startScoring();
    const { client, requests } = api(3, { pageSize: 2 });
    const result = await evaluate({ dataset: "faq", task: () => 1, client, experimentName: "x" });
    const pages = requests().filter((request) => request.url.includes("/datasets/d1/items"));
    expect(pages).toHaveLength(1);
    expect(pages[0]?.url.endsWith("/datasets/d1/items?version=2&limit=1000&cursor=c1")).toBe(true);
    expect(result.runs.map((run) => run.datasetItemId)).toEqual(["item-1", "item-2", "item-3"]);
  });

  it("makes each run its own trace with the experiment linkage", async () => {
    const { exporter } = startScoring({ environment: "prod" });
    const { client } = api(2);
    const result = await evaluate({
      dataset: "faq",
      task: (input) => withSpan("inner", () => input),
      client,
      experimentName: "exp",
      repetitions: 2,
    });
    expect(result.runs).toHaveLength(4);
    const spans = await finished(exporter);
    const roots = spans.filter((span) => span.parentSpanContext === undefined);
    expect(roots).toHaveLength(4);
    expect(roots.every((span) => span.name === "exp")).toBe(true);
    for (const span of spans) {
      expect(span.attributes["cetra.experiment.id"]).toBe("e1");
      expect(span.attributes["cetra.environment"]).toBe("experiment");
    }
    expect(roots.map((span) => span.attributes["cetra.dataset_item.id"]).sort()).toEqual([
      "item-1",
      "item-1",
      "item-2",
      "item-2",
    ]);
  });

  it("does not leak the caller's context into the runs", async () => {
    const { exporter } = startScoring();
    const { client } = api(1);
    await withContext({ sessionId: "s-1" }, () =>
      evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" }),
    );
    const roots = (await finished(exporter)).filter((span) => span.parentSpanContext === undefined);
    expect(roots).toHaveLength(1);
    expect(roots[0]?.attributes["cetra.session.id"]).toBeUndefined();
  });

  it("records a task error on its row and span, and runs no scorer for it", async () => {
    const { exporter } = startScoring();
    const { client, requests } = api(2);
    const scoreCalls: ScoreCall[] = [];
    const scorerRan = vi.fn(() => true);
    const result = await evaluate({
      dataset: "faq",
      task: (input) => {
        if ((input as { n: number }).n === 1) {
          throw new TypeError("boom");
        }
        return "ok";
      },
      scorers: [scorerRan],
      client,
      experimentName: "exp",
    });
    expect(result.errorCount).toBe(1);
    expect(result.runs[0]?.error).toBe("TypeError: boom");
    expect(result.runs[0]?.output).toBeUndefined();
    expect(scorerRan).toHaveBeenCalledTimes(1);
    const rows = resultRows(requests());
    expect(rows[0]).toMatchObject({ dataset_item_id: "item-1", error: "TypeError: boom" });
    expect(rows[0]).not.toHaveProperty("output");
    const failedRoots = (await finished(exporter)).filter(
      (span) => span.parentSpanContext === undefined && span.status.code === SpanStatusCode.ERROR,
    );
    expect(failedRoots).toHaveLength(1);
    expect(failedRoots[0]?.events.some((event) => event.name === "exception")).toBe(true);
    expect(scoresOf(scoreCalls)).toEqual([]);
  });

  it("passes the scorer arguments by name and queues each score", async () => {
    const calls: ScoreCall[] = [];
    startScoring({}, calls);
    const { client } = api(1);
    const scorers = [
      function exact({ output, expected }: ScorerArgs) {
        return output === expected;
      },
      function flag(): boolean {
        return true;
      },
      function ratio(): number {
        return 0.5;
      },
      function label(): string {
        return "good";
      },
      function rich(): Score {
        return { value: 1, reason: "r", metadata: { k: "v" } };
      },
      function nothing(): undefined {
        return undefined;
      },
    ];
    const result = await evaluate({
      dataset: "faq",
      task: (input) => (input as { n: number }).n,
      scorers,
      client,
      experimentName: "exp",
    });
    expect(result.runs[0]?.scores.exact).toEqual({ value: true });
    expect(result.runs[0]?.scores.nothing).toBeUndefined();
    await flush();
    const queued = scoresOf(calls);
    expect(queued.map((score) => score.name).sort()).toEqual([
      "exact",
      "flag",
      "label",
      "ratio",
      "rich",
    ]);
    const traceId = result.runs[0]?.traceId;
    const rich = queued.find((score) => score.name === "rich");
    expect(rich).toMatchObject({
      target: { type: "trace", trace_id: traceId },
      value: { data_type: "numeric", value: 1 },
      reason: "r",
      metadata: { k: "v" },
      experiment_id: "e1",
      dataset_item_id: "item-1",
      source: "sdk",
    });
    expect(queued.find((score) => score.name === "label")?.value).toEqual({
      data_type: "categorical",
      value: { label: "good" },
    });
    expect(queued.find((score) => score.name === "flag")?.value).toEqual({
      data_type: "boolean",
      value: true,
    });
  });

  it("counts scorer errors and warns once per scorer", async () => {
    startScoring();
    const { client } = api(4);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const bad = function bad(): never {
      throw new RangeError("secret-canary");
    };
    const result = await evaluate({
      dataset: "faq",
      task: () => 1,
      scorers: [bad],
      client,
      experimentName: "exp",
    });
    expect(result.scorerErrors).toEqual({ bad: 4 });
    const messages = warn.mock.calls.map((call) => String(call[0]));
    expect(
      messages.filter((message) => message === "cetra: scorer 0 failed (RangeError)"),
    ).toHaveLength(1);
    expect(messages.some((message) => message.includes("secret-canary"))).toBe(false);
  });

  it("sends the results in batches of 500 and finishes last", async () => {
    startScoring();
    const { client, requests } = api(1200);
    await evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" });
    const posts = requests().filter((request) => request.url.endsWith("/experiments/e1/items"));
    const counts = posts.map(
      (request) => (JSON.parse(request.body ?? "{}") as { results: unknown[] }).results.length,
    );
    expect(counts).toEqual([500, 500, 200]);
    const order = requests().map(
      (request) => request.url.split("/api/v1/projects/current")[1] ?? request.url,
    );
    expect(order.at(-1)).toBe("/experiments/e1/finish");
  });

  it("numbers the repetitions from 0", async () => {
    startScoring();
    const { client, requests } = api(1);
    await evaluate({
      dataset: "faq",
      task: () => 1,
      client,
      experimentName: "exp",
      repetitions: 3,
    });
    const reps = resultRows(requests()).map((row) => row.repetition);
    expect(reps).toEqual([0, 1, 2]);
  });

  it("does not retry the create", async () => {
    startScoring();
    const { client, requests, rest } = api(1, {
      override: (request, path) =>
        request.method === "POST" && path === "/experiments"
          ? { status: 503, json: { code: "unavailable" } }
          : undefined,
    });
    await expect(
      evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" }),
    ).rejects.toMatchObject({ status: 503 });
    expect(requests()).toHaveLength(1);
    expect(rest.sleeps).toEqual([]);
  });

  it("retries the finish and succeeds", async () => {
    startScoring();
    let finishes = 0;
    const { client, requests, rest } = api(1, {
      override: (request, path) => {
        if (request.method === "POST" && path === "/experiments/e1/finish") {
          finishes += 1;
          return finishes === 1 ? { status: 503, json: { code: "unavailable" } } : undefined;
        }
        return undefined;
      },
    });
    await evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" });
    expect(requests().filter((request) => request.url.endsWith("/finish"))).toHaveLength(2);
    expect(rest.sleeps).toEqual([1000]);
  });

  it("cuts an output over 256 KiB to 260,000 bytes with a marker", async () => {
    startScoring();
    const { client, requests } = api(1);
    await evaluate({
      dataset: "faq",
      task: () => "x".repeat(300_000),
      client,
      experimentName: "exp",
    });
    const [row] = resultRows(requests());
    const output = row?.output as string;
    expect(output.startsWith(`"${"x".repeat(259_999)}`)).toBe(true);
    expect(output.endsWith("…[cetra: truncated, original 300002 bytes]")).toBe(true);
  });

  it("finishes with an error and raises when the results upload fails", async () => {
    startScoring();
    let posts = 0;
    const { client, requests } = api(501, {
      override: (request, path) => {
        if (request.method === "POST" && path === "/experiments/e1/items") {
          posts += 1;
          return posts === 2 ? { status: 422, json: { code: "invalid" } } : undefined;
        }
        return undefined;
      },
    });
    await expect(
      evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" }),
    ).rejects.toBeInstanceOf(ApiError);
    const finishes = bodiesTo(requests(), "POST", "/finish");
    expect(finishes).toEqual([{ error: "results upload failed" }]);
  });

  it("warns once when sample_rate is below 1", async () => {
    startScoring({ sampleRate: 0.5 });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { client } = api(1);
    await evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" });
    const messages = warn.mock.calls.map((call) => String(call[0]));
    expect(
      messages.filter(
        (message) =>
          message ===
          "cetra: sample_rate is below 1; some experiment runs will have no trace and no score",
      ),
    ).toHaveLength(1);
  });

  it("warns once when the 402 pause drops scores, and keeps running", async () => {
    const calls: ScoreCall[] = [];
    // Every score batch answers 402: the first one pauses the queue.
    startMemory({}, {}, { fetch: scoreFetch(calls, 402) });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    score({ name: "pre", value: 1, traceId: "0123456789abcdef0123456789abcdef" });
    await flush();
    const { client, requests } = api(2);
    const result = await evaluate({
      dataset: "faq",
      task: () => 1,
      client,
      experimentName: "exp",
      scorers: [
        function ok(): boolean {
          return true;
        },
      ],
    });
    expect(result.runs).toHaveLength(2);
    expect(result.runs[0]?.scores.ok).toEqual({ value: true });
    expect(requests().some((request) => request.url.endsWith("/experiments/e1/finish"))).toBe(true);
    const messages = warn.mock.calls.map((call) => String(call[0]));
    expect(
      messages.filter(
        (message) =>
          message ===
          "cetra: the monthly units cap paused score uploads; experiment scores are being dropped",
      ),
    ).toHaveLength(1);
  });

  it("keeps payload content and secrets out of the logs", async () => {
    startScoring();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const debug = vi.spyOn(diag, "debug").mockImplementation(() => {});
    const { client } = api(2);
    await evaluate({
      dataset: "faq",
      task: (input) => {
        if ((input as { n: number }).n === 1) {
          throw new Error("secret-canary");
        }
        return 1;
      },
      scorers: [
        function leaky(): never {
          throw new Error("secret-canary-2");
        },
      ],
      client,
      experimentName: "exp",
    });
    expect(warn.mock.calls.map((call) => String(call[0]))).toContain(
      "cetra: scorer 0 failed (Error)",
    );
    const logged = [...warn.mock.calls, ...debug.mock.calls].map((call) =>
      call.map((part) => String(part)).join(" "),
    );
    expect(logged.some((line) => line.includes("secret-canary"))).toBe(false);
  });
});

describe("ExperimentResult", () => {
  function run(scores: Record<string, Score>, error?: string): Run {
    return {
      datasetItemId: "i",
      repetition: 0,
      input: null,
      expectedOutput: null,
      output: null,
      error,
      traceId: undefined,
      latencyNs: 1,
      scores,
    };
  }

  const result = new ExperimentResult({
    experimentId: "e1",
    experimentName: "exp",
    datasetId: "d1",
    datasetVersion: 2,
    runs: [
      run({ exact: { value: true }, tag: { value: "b" } }),
      run({ exact: { value: false }, tag: { value: "a" } }),
      run({ exact: { value: true }, tag: { value: "b" } }),
    ],
    scorerErrors: { tag: 1 },
    scorerNames: ["exact", "tag", "unused"],
  });

  it("prints one line per scorer", () => {
    expect(result.summary()).toBe(
      [
        "Experiment exp (e1) on dataset d1 v2: 3 runs, 0 errors",
        "  exact: mean 0.667 (3 values)",
        "  tag: b 2, a 1",
        "  tag: 1 errors",
        "  unused: no values",
      ].join("\n"),
    );
  });

  it("asserts a minimum mean", () => {
    expect(() => result.assertMin("exact", 0.5)).not.toThrow();
    expect(() => result.assertMin("exact", 0.9)).toThrow("exact: mean 0.667 is below 0.9");
    expect(() => result.assertMin("unused", 0)).toThrow("unused: no numeric or boolean values");
    expect(() => result.assertMin("missing", 0)).toThrow("missing: no numeric or boolean values");
  });
});

describe("evaluate: scopes, failures and limits", () => {
  it("runs the scorers in the experiment scope, without the caller's user and session", async () => {
    const calls: ScoreCall[] = [];
    const { exporter } = startScoring({}, calls);
    const { client } = api(1);
    const spanScorer = function spanScorer(): boolean {
      withSpan("scorer-span", () => undefined);
      return true;
    };
    await withContext({ userId: "u-1", sessionId: "s-1" }, () =>
      withSpan("caller", () =>
        evaluate({
          dataset: "faq",
          task: () => 1,
          scorers: [spanScorer],
          client,
          experimentName: "exp",
        }),
      ),
    );
    await flush();
    const scorerSpan = (await finished(exporter)).find((span) => span.name === "scorer-span");
    expect(scorerSpan?.attributes["cetra.experiment.id"]).toBe("e1");
    expect(scorerSpan?.attributes["cetra.dataset_item.id"]).toBe("item-1");
    expect(scorerSpan?.attributes["cetra.session.id"]).toBeUndefined();
    const queued = scoresOf(calls);
    expect(queued).toHaveLength(1);
    expect(queued[0]).not.toHaveProperty("user_id");
    expect(queued[0]).not.toHaveProperty("session_id");
  });

  it("finishes with the error's name and rejects when an item page fails", async () => {
    startScoring();
    const { client, requests } = api(3, {
      pageSize: 2,
      override: (request, path) =>
        request.method === "GET" && path === "/datasets/d1/items"
          ? { status: 422, json: { code: "invalid" } }
          : undefined,
    });
    await expect(
      evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(bodiesTo(requests(), "POST", "/finish")).toEqual([{ error: "ApiError" }]);
    expect(bodiesTo(requests(), "POST", "/experiments/e1/items")).toEqual([]);
  });

  it("finishes with the error's name and rejects when a worker fails unexpectedly", async () => {
    startScoring();
    const { client, requests } = api(3);
    // A scorer whose return value cannot be read: normalizing it throws outside the scorer's own try.
    const hostile = function hostile(): Score {
      return {
        get value(): never {
          throw new RangeError("getter");
        },
      };
    };
    await expect(
      evaluate({
        dataset: "faq",
        task: () => 1,
        scorers: [hostile],
        client,
        experimentName: "exp",
        concurrency: 2,
      }),
    ).rejects.toThrow("getter");
    expect(bodiesTo(requests(), "POST", "/finish")).toEqual([{ error: "RangeError" }]);
  });

  it("raises when the finish fails, and sends no second finish", async () => {
    startScoring();
    const { client, requests } = api(1, {
      override: (request, path) =>
        request.method === "POST" && path === "/experiments/e1/finish"
          ? { status: 409, json: { code: "conflict" } }
          : undefined,
    });
    await expect(
      evaluate({ dataset: "faq", task: () => 1, client, experimentName: "exp" }),
    ).rejects.toMatchObject({ status: 409 });
    expect(bodiesTo(requests(), "POST", "/finish")).toHaveLength(1);
  });

  it("stops starting new runs after an upload failure", async () => {
    startScoring();
    const task = vi.fn(() => 1);
    const { client, requests } = api(1200, {
      override: (request, path) =>
        request.method === "POST" && path === "/experiments/e1/items"
          ? { status: 422, json: { code: "invalid" } }
          : undefined,
    });
    await expect(
      evaluate({ dataset: "faq", task, client, experimentName: "exp", concurrency: 1 }),
    ).rejects.toBeInstanceOf(ApiError);
    // The first batch is 500 rows: the run that filled it is the last one started.
    expect(task).toHaveBeenCalledTimes(500);
    expect(bodiesTo(requests(), "POST", "/finish")).toEqual([{ error: "results upload failed" }]);
  });

  it("runs at most concurrency tasks at once", async () => {
    startScoring();
    const { client } = api(8);
    let running = 0;
    let peak = 0;
    await evaluate({
      dataset: "faq",
      task: async () => {
        running += 1;
        peak = Math.max(peak, running);
        await new Promise((resolve) => setTimeout(resolve, 5));
        running -= 1;
        return 1;
      },
      client,
      experimentName: "exp",
      concurrency: 2,
    });
    expect(peak).toBeGreaterThan(0);
    expect(peak).toBeLessThanOrEqual(2);
  });

  it("sends at most 32 MiB of outputs in a batch", async () => {
    startScoring();
    const { client, requests } = api(300);
    await evaluate({
      dataset: "faq",
      task: () => "x".repeat(200_000),
      client,
      experimentName: "exp",
    });
    // Each row's output is 200,002 bytes of JSON: 167 rows fit in 32 MiB, 168 do not.
    const counts = bodiesTo(requests(), "POST", "/experiments/e1/items").map(
      (body) => (body as { results: unknown[] }).results.length,
    );
    expect(counts).toEqual([167, 133]);
  });

  it("omits the version of a dataset at version 0", async () => {
    startScoring();
    const { client, requests } = api(1);
    const empty = new Dataset(
      { id: "d1", name: "faq", description: undefined, version: 0, items: [] },
      client,
    );
    await evaluate({ dataset: empty, task: () => 1, client, experimentName: "exp" });
    const [create] = bodiesTo(requests(), "POST", "/experiments") as Record<string, unknown>[];
    expect(create).toEqual({ name: "exp", dataset: "d1", repetitions: 1 });
  });

  it("records a thrown error whose message cannot be read by its type name", async () => {
    startScoring();
    const { client, requests } = api(1);
    const unreadable = new TypeError("never read");
    Object.defineProperty(unreadable, "message", {
      get: (): never => {
        throw new RangeError("getter");
      },
    });
    const result = await evaluate({
      dataset: "faq",
      task: () => {
        throw unreadable;
      },
      client,
      experimentName: "exp",
    });
    expect(result.runs[0]?.error).toBe("TypeError");
    expect(resultRows(requests())[0]).toMatchObject({ error: "TypeError" });
  });
});
