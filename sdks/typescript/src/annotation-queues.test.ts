import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import type { QueueTargets } from "./annotation-queues.js";
import { ApiError, Client } from "./client.js";
import { BASE, FakeRest, inOrder, type RecordedRequest } from "./rest.test-helpers.js";

function clientFor(rest: FakeRest): Client {
  return new Client({
    apiKey: "sk-cetra-test",
    baseUrl: "http://cetra.test",
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

/** A committed fixture of `fixtures/api/static`, read from disk. */
function fixture(name: string): unknown {
  return JSON.parse(
    readFileSync(new URL(`../../../fixtures/api/static/${name}`, import.meta.url), "utf8"),
  ) as unknown;
}

const QUEUE_ID = "01900000-0000-7000-8000-000000000a01";
const DEFINITION_ID = "01900000-0000-7000-8000-000000000600";
const TRACE_ID = "4bf92f3577b34da6a3ce929d0e0e4736";
const SPAN_ID = "00f067aa0ba902b7";

const queueFixture = fixture("annotation-queue.json") as Record<string, unknown>;

/** Targets that the type forbids, for the runtime checks. */
function unchecked(value: unknown): QueueTargets {
  return value as QueueTargets;
}

describe("annotation queues", () => {
  it("createSendsTheBodyAndMapsTheQueue", async () => {
    const rest = new FakeRest(inOrder({ status: 201, json: queueFixture }));
    const queue = await clientFor(rest).annotationQueues.create("Weekly review", {
      target: "trace",
      definitionIds: [DEFINITION_ID],
    });
    const [request] = rest.requests;
    expect(route(request as RecordedRequest)).toEqual({
      method: "POST",
      path: "/annotation-queues",
      query: "",
    });
    expect(JSON.parse(request?.body ?? "")).toEqual({
      name: "Weekly review",
      target: "trace",
      definition_ids: [DEFINITION_ID],
    });
    expect(queue).toMatchObject({
      id: QUEUE_ID,
      name: "Relevance review",
      description: "Support bot traces that the relevance judge scored low.",
      instructions:
        "Score each answer against the question it answers. Skip the traces that have no question.",
      target: "trace",
      definitionIds: [DEFINITION_ID],
      version: 2,
      createdBy: "01900000-0000-7000-8000-000000000100",
      createdAt: "2026-01-01T09:00:00.000000000Z",
      updatedAt: "2026-01-01T10:30:00.000000000Z",
      counts: { pending: 12, completed: 4, skipped: 1 },
    });
  });

  it("createRejectsAnUnknownTargetOrNoDefinitions", async () => {
    const rest = new FakeRest(inOrder());
    const client = clientFor(rest);
    await expect(
      client.annotationQueues.create("Weekly review", {
        target: "video" as unknown as "trace",
        definitionIds: [DEFINITION_ID],
      }),
    ).rejects.toThrow(TypeError);
    await expect(
      client.annotationQueues.create("Weekly review", { target: "trace", definitionIds: [] }),
    ).rejects.toThrow(TypeError);
    expect(rest.requests).toHaveLength(0);
  });

  it("listFollowsCursors", async () => {
    const page = fixture("annotation-queues.json") as { data: unknown[] };
    const second = { ...queueFixture, id: "01900000-0000-7000-8000-000000000a02", name: "Second" };
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: { data: page.data, next_cursor: "p2" } },
        { status: 200, json: { data: [second], next_cursor: null } },
      ),
    );
    const queues = await clientFor(rest).annotationQueues.list();
    expect(rest.requests[0]?.url).toBe(`${BASE}/annotation-queues?limit=1000`);
    expect(rest.requests[1]?.url).toBe(`${BASE}/annotation-queues?limit=1000&cursor=p2`);
    expect(queues.map((queue) => queue.id)).toEqual([QUEUE_ID, second.id]);
    expect(queues[1]?.name).toBe("Second");
  });

  it("getUpdateDelete", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: queueFixture },
        { status: 200, json: { ...queueFixture, name: "Renamed", version: 3 } },
        { status: 204 },
      ),
    );
    const client = clientFor(rest);

    const got = await client.annotationQueues.get(QUEUE_ID);
    expect(got.id).toBe(QUEUE_ID);
    expect(route(rest.requests[0] as RecordedRequest)).toEqual({
      method: "GET",
      path: `/annotation-queues/${QUEUE_ID}`,
      query: "",
    });

    const updated = await client.annotationQueues.update(QUEUE_ID, {
      version: 2,
      name: "Renamed",
    });
    expect(updated).toMatchObject({ name: "Renamed", version: 3 });
    const patch = rest.requests[1] as RecordedRequest;
    expect(route(patch)).toEqual({
      method: "PATCH",
      path: `/annotation-queues/${QUEUE_ID}`,
      query: "",
    });
    expect(JSON.parse(patch.body ?? "")).toEqual({ version: 2, name: "Renamed" });

    await expect(client.annotationQueues.delete(QUEUE_ID)).resolves.toBeUndefined();
    expect(route(rest.requests[2] as RecordedRequest)).toEqual({
      method: "DELETE",
      path: `/annotation-queues/${QUEUE_ID}`,
      query: "",
    });
  });

  it("addItemsShapes", async () => {
    const added = fixture("annotation-queue-items-added.json") as {
      added: number;
      existing: number;
    };
    const rest = new FakeRest(
      inOrder(
        { status: 201, json: added },
        { status: 201, json: added },
        { status: 201, json: added },
      ),
    );
    const client = clientFor(rest);
    const result = await client.annotationQueues.addItems(QUEUE_ID, { traceIds: [TRACE_ID] });
    expect(result.added).toBe(added.added);
    expect(result.existing).toBe(added.existing);
    await client.annotationQueues.addItems(QUEUE_ID, { spans: [[TRACE_ID, SPAN_ID]] });
    await client.annotationQueues.addItems(QUEUE_ID, { sessionIds: ["chat-42"] });

    expect(route(rest.requests[0] as RecordedRequest)).toEqual({
      method: "POST",
      path: `/annotation-queues/${QUEUE_ID}/items`,
      query: "",
    });
    expect(rest.requests.map((request) => JSON.parse(request.body ?? "{}"))).toEqual([
      { items: [{ trace_id: TRACE_ID }] },
      { items: [{ trace_id: TRACE_ID, span_id: SPAN_ID }] },
      { items: [{ session_id: "chat-42" }] },
    ]);
  });

  it("addItemsNeedsExactlyOneKind", async () => {
    const rest = new FakeRest(inOrder());
    const client = clientFor(rest);
    await expect(client.annotationQueues.addItems(QUEUE_ID, unchecked({}))).rejects.toThrow(
      TypeError,
    );
    await expect(
      client.annotationQueues.addItems(
        QUEUE_ID,
        unchecked({ traceIds: [TRACE_ID], sessionIds: ["chat-42"] }),
      ),
    ).rejects.toThrow(TypeError);
    await expect(
      client.annotationQueues.addItems(QUEUE_ID, unchecked({ traceIds: TRACE_ID })),
    ).rejects.toThrow(TypeError);
    await expect(
      client.annotationQueues.addItems(QUEUE_ID, unchecked({ traceIds: [TRACE_ID], extra: [] })),
    ).rejects.toThrow(TypeError);
    await expect(
      client.annotationQueues.addItems(QUEUE_ID, unchecked({ spans: [[TRACE_ID]] })),
    ).rejects.toThrow(TypeError);
    expect(rest.requests).toHaveLength(0);
  });

  it("addItemsBatchesAndOffsetsSkipped", async () => {
    const traces = Array.from({ length: 1_500 }, (_, index) =>
      index.toString(16).padStart(32, "0"),
    );
    const rest = new FakeRest(
      inOrder(
        { status: 201, json: { added: 1_000, existing: 0, skipped: [] } },
        {
          status: 201,
          json: { added: 499, existing: 0, skipped: [{ index: 3, message: "unknown trace" }] },
        },
      ),
    );
    const result = await clientFor(rest).annotationQueues.addItems(QUEUE_ID, {
      traceIds: traces,
    });
    const sizes = rest.requests.map(
      (request) => (JSON.parse(request.body ?? "{}") as { items: unknown[] }).items.length,
    );
    expect(sizes).toEqual([1_000, 500]);
    expect(result).toEqual({
      added: 1_499,
      existing: 0,
      skipped: [{ index: 1_003, message: "unknown trace" }],
    });
  });

  it("addItemsEmptySendsNothing", async () => {
    const rest = new FakeRest(inOrder());
    const result = await clientFor(rest).annotationQueues.addItems(QUEUE_ID, { traceIds: [] });
    expect(result).toEqual({ added: 0, existing: 0, skipped: [] });
    expect(rest.requests).toHaveLength(0);
  });

  it("itemsFiltersByStatus", async () => {
    const items = fixture("annotation-queue-items.json") as {
      data: { id: string; status: string }[];
    };
    const rest = new FakeRest(inOrder({ status: 200, json: items }));
    const result = await clientFor(rest).annotationQueues.items(QUEUE_ID, { status: "pending" });
    expect(rest.requests[0]?.url).toBe(
      `${BASE}/annotation-queues/${QUEUE_ID}/items?status=pending&limit=1000`,
    );
    expect(result.map((item) => item.id)).toEqual(items.data.map((item) => item.id));
    expect(result.map((item) => item.status)).toEqual(items.data.map((item) => item.status));
  });

  it("errorsAreApiErrors", async () => {
    const rest = new FakeRest(
      inOrder(
        {
          status: 403,
          json: { code: "plan_limit", detail: "the plan has no annotation queues", errors: [] },
        },
        { status: 409, json: { detail: "the queue changed", errors: [] } },
      ),
    );
    const client = clientFor(rest);
    const refused = await client.annotationQueues
      .create("Weekly review", { target: "trace", definitionIds: [DEFINITION_ID] })
      .catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(ApiError);
    expect(refused).toMatchObject({ code: "plan_limit", status: 403 });

    await expect(
      client.annotationQueues.update(QUEUE_ID, { version: 1, name: "Stale" }),
    ).rejects.toMatchObject({ status: 409 });
  });
});
