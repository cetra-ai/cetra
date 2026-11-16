import { describe, expect, it } from "vitest";

import { ApiError, Client } from "./client.js";
import type { NewDatasetItem } from "./datasets.js";
import { BASE, FakeRest, inOrder, type RecordedRequest } from "./rest.test-helpers.js";

function clientFor(rest: FakeRest): Client {
  return new Client({
    apiKey: "sk-llmobs-test",
    baseUrl: "http://llmobs.test",
    fetch: rest.fetch,
    sleep: rest.sleep,
  });
}

/** Path below the project base, with the query string split off. */
function route(request: RecordedRequest): { method: string; path: string; query: string } {
  const rest = request.url.slice(BASE.length);
  const [path = "", query = ""] = rest.split("?");
  return { method: request.method, path, query };
}

const DATASET = {
  id: "d1",
  name: "faq",
  description: "answers",
  current_version: 0,
  item_count: 0,
  experiment_count: 0,
};

function item(id: string): Record<string, unknown> {
  return {
    id,
    revision_id: `r-${id}`,
    version: 1,
    input: `q-${id}`,
    expected_output: `a-${id}`,
    metadata: {},
    created_at: "2026-01-01T00:00:00.000000000Z",
  };
}

describe("datasets", () => {
  it("createPostsTheFieldsAndReturnsVersion0", async () => {
    const rest = new FakeRest(inOrder({ status: 201, json: DATASET }));
    const dataset = await clientFor(rest).datasets.create("faq", {
      description: "answers",
      inputSchema: { type: "object" },
    });
    const [request] = rest.requests;
    expect(route(request as RecordedRequest)).toEqual({
      method: "POST",
      path: "/datasets",
      query: "",
    });
    expect(JSON.parse(request?.body ?? "")).toEqual({
      name: "faq",
      description: "answers",
      input_schema: { type: "object" },
    });
    expect(dataset).toMatchObject({ id: "d1", name: "faq", version: 0 });
    expect(dataset.items).toEqual([]);
  });

  it("getPagesItemsAndPinsTheVersion", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: { ...DATASET, current_version: 3 } },
        { status: 200, json: { data: [item("i1"), item("i2")], next_cursor: "c1" } },
        { status: 200, json: { data: [item("i3")], next_cursor: null } },
      ),
    );
    const dataset = await clientFor(rest).datasets.get("faq");
    expect(dataset.version).toBe(3);
    expect(dataset.items.map((entry) => entry.id)).toEqual(["i1", "i2", "i3"]);
    expect(dataset.items[0]).toMatchObject({
      input: "q-i1",
      expectedOutput: "a-i1",
      metadata: {},
      version: 1,
      sourceTraceId: undefined,
    });
    expect(rest.requests[2]?.url).toBe(`${BASE}/datasets/d1/items?version=3&limit=1000&cursor=c1`);
  });

  it("getEncodesAName", async () => {
    const rest = new FakeRest(
      inOrder({ status: 404, json: { code: "not_found", detail: "no dataset", errors: [] } }),
    );
    await expect(clientFor(rest).datasets.get("faq v1/x")).rejects.toBeInstanceOf(ApiError);
    expect(rest.requests[0]?.url).toBe(`${BASE}/datasets/faq%20v1%2Fx`);
  });

  it("addItemsBatchesBy1000", async () => {
    const answer = (request: RecordedRequest) => {
      const { method, path } = route(request);
      if (method === "POST" && path === "/datasets") {
        return { status: 201, json: DATASET };
      }
      if (method === "POST" && path === "/datasets/d1/items") {
        return { status: 201, json: { version: 1, item_ids: [] } };
      }
      if (method === "GET" && path === "/datasets/d1") {
        return { status: 200, json: { ...DATASET, current_version: 3 } };
      }
      return { status: 200, json: { data: [], next_cursor: null } };
    };
    const rest = new FakeRest(answer);
    const dataset = await clientFor(rest).datasets.create("faq");
    const items: NewDatasetItem[] = Array.from({ length: 2_500 }, (_, index) => ({ input: index }));
    const latest = await dataset.addItems(items);

    const posts = rest.requests.filter(
      (request) => request.method === "POST" && request.url.endsWith("/items"),
    );
    expect(
      posts.map(
        (request) => (JSON.parse(request.body ?? "{}") as { items: unknown[] }).items.length,
      ),
    ).toEqual([1_000, 1_000, 500]);
    expect(rest.requests.at(-1)?.url).toBe(`${BASE}/datasets/d1/items?version=3&limit=1000`);
    expect(latest.version).toBe(3);
  });

  it("addItemsRejectsUnknownKeys", async () => {
    const rest = new FakeRest(inOrder({ status: 201, json: DATASET }));
    const dataset = await clientFor(rest).datasets.create("faq");
    const before = rest.requests.length;
    const bad = [{ input: 1, answer: 2 }] as unknown as NewDatasetItem[];
    await expect(dataset.addItems(bad)).rejects.toThrow(TypeError);
    expect(rest.requests).toHaveLength(before);
  });

  it("listPagesAndDeleteSendsDelete", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: { data: [DATASET], next_cursor: "p2" } },
        {
          status: 200,
          json: {
            data: [{ ...DATASET, id: "d2", name: "other", current_version: 2, item_count: 5 }],
            next_cursor: null,
          },
        },
        { status: 204 },
      ),
    );
    const client = clientFor(rest);
    const infos = await client.datasets.list();
    expect(rest.requests[0]?.url).toBe(`${BASE}/datasets?limit=1000`);
    expect(rest.requests[1]?.url).toBe(`${BASE}/datasets?limit=1000&cursor=p2`);
    expect(infos).toEqual([
      { id: "d1", name: "faq", description: "answers", currentVersion: 0, itemCount: 0 },
      { id: "d2", name: "other", description: "answers", currentVersion: 2, itemCount: 5 },
    ]);
    await client.datasets.delete("faq");
    expect(route(rest.requests[2] as RecordedRequest)).toEqual({
      method: "DELETE",
      path: "/datasets/faq",
      query: "",
    });
  });

  it("getNotFoundRaises", async () => {
    const rest = new FakeRest(
      inOrder({ status: 404, json: { code: "not_found", detail: "no dataset", errors: [] } }),
    );
    await expect(clientFor(rest).datasets.get("missing")).rejects.toMatchObject({ status: 404 });
  });
});
