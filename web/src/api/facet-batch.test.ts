import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { askFacet, type FacetShared } from "./facet-batch";
import { problemOf } from "./problem";
import type { Schema } from "./types";

const shared: FacetShared = {
  view: "spans",
  from: "2026-11-18T10:00:00.000Z",
  to: "2026-11-18T11:00:00.000Z",
};

const facet = (count: number): Schema<"Facet"> => ({
  kind: "values",
  total: count,
  missing: 0,
  values: [{ value: "llm", count }],
  more: false,
});

const problem = {
  type: "about:blank",
  title: "Bad Request",
  status: 400,
  code: "bad_request",
  detail: "q must be 1 to 200 characters",
};

type Body = Schema<"FacetsRequest">;

/** Records each request body and answers every field with `facet(count)`, or the given result. */
function recordRequests(answer: (body: Body) => Response | Promise<Response>) {
  const bodies: Body[] = [];
  server.use(
    http.post("*/api/v1/projects/:project/facets", async ({ request }) => {
      const body = (await request.json()) as Body;
      bodies.push(body);
      return answer(body);
    }),
  );
  return bodies;
}

const answerAll = (body: Body) =>
  HttpResponse.json({
    view: body.view,
    results: body.fields.map((field, index) => ({
      field: field.field,
      facet: facet(index + 1),
    })),
  });

describe("askFacet", () => {
  it("fields asked together go out as one request", async () => {
    const bodies = recordRequests(answerAll);
    const asks = [
      askFacet("acme/support-bot", shared, { field: "span_kind", limit: 10 }),
      askFacet("acme/support-bot", shared, { field: "name", limit: 10 }),
      askFacet("acme/support-bot", shared, { field: "duration_ms" }),
    ];
    const [first, second, third] = await Promise.all(asks);
    expect(bodies).toHaveLength(1);
    expect(bodies[0]?.fields).toEqual([
      { field: "span_kind", limit: 10 },
      { field: "name", limit: 10 },
      { field: "duration_ms" },
    ]);
    expect(first?.total).toBe(1);
    expect(second?.total).toBe(2);
    expect(third?.total).toBe(3);
  });

  it("a field error rejects only that field", async () => {
    recordRequests((body) =>
      HttpResponse.json({
        view: body.view,
        results: body.fields.map((field, index) =>
          index === 1
            ? { field: field.field, error: problem }
            : { field: field.field, facet: facet(index + 1) },
        ),
      }),
    );
    const first = askFacet("acme/support-bot", shared, { field: "span_kind", limit: 10 });
    const second = askFacet("acme/support-bot", shared, { field: "name", limit: 10 });
    const third = askFacet("acme/support-bot", shared, { field: "duration_ms" });
    await expect(first).resolves.toMatchObject({ total: 1 });
    await expect(third).resolves.toMatchObject({ total: 3 });
    const rejected = await second.then(
      () => undefined,
      (error: unknown) => error,
    );
    expect(problemOf(rejected)).toMatchObject({ code: "bad_request", status: 400 });
  });

  it("a whole-request problem rejects every field", async () => {
    recordRequests(() => HttpResponse.json(problem, { status: 400 }));
    const asks = [
      askFacet("acme/support-bot", shared, { field: "span_kind", limit: 10 }),
      askFacet("acme/support-bot", shared, { field: "name", limit: 10 }),
    ];
    for (const ask of asks) {
      const rejected = await ask.then(
        () => undefined,
        (error: unknown) => error,
      );
      expect(problemOf(rejected)?.code).toBe("bad_request");
    }
  });

  it("different shared parts go out separately", async () => {
    const bodies = recordRequests(answerAll);
    await Promise.all([
      askFacet("acme/support-bot", shared, { field: "span_kind" }),
      askFacet("acme/support-bot", { ...shared, filter: "status = 'error'" }, { field: "name" }),
    ]);
    expect(bodies).toHaveLength(2);
    expect(bodies.map((body) => body.fields)).toEqual([
      [{ field: "span_kind" }],
      [{ field: "name" }],
    ]);
    expect(bodies.map((body) => body.filter)).toEqual([undefined, "status = 'error'"]);
  });

  it("the same field asked twice is sent once", async () => {
    const bodies = recordRequests(answerAll);
    const [first, second] = await Promise.all([
      askFacet("acme/support-bot", shared, { field: "span_kind", limit: 10 }),
      askFacet("acme/support-bot", shared, { field: "span_kind", limit: 10 }),
    ]);
    expect(bodies).toHaveLength(1);
    expect(bodies[0]?.fields).toEqual([{ field: "span_kind", limit: 10 }]);
    expect(first).toEqual(second);
    expect(first?.total).toBe(1);
  });

  it("a later ask starts a new batch", async () => {
    const bodies = recordRequests(answerAll);
    await askFacet("acme/support-bot", shared, { field: "span_kind" });
    await askFacet("acme/support-bot", shared, { field: "name" });
    expect(bodies).toHaveLength(2);
  });

  it("more than 20 fields go out in several requests", async () => {
    const bodies = recordRequests(answerAll);
    const asks = Array.from({ length: 21 }, (_, index) =>
      askFacet("acme/support-bot", shared, { field: `f${index}` }),
    );
    const results = await Promise.all(asks);
    expect(bodies.map((body) => body.fields.length)).toEqual([20, 1]);
    expect(results).toHaveLength(21);
    for (const result of results) {
      expect(result).toBeDefined();
    }
  });
});
