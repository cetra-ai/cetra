import { afterEach, describe, expect, it, vi } from "vitest";

import { ApiError, Client } from "./client.js";
import { ConfigError } from "./config.js";
import { BASE, FakeRest, inOrder } from "./rest.test-helpers.js";

const KEY = "sk-cetra-test";

function clientFor(rest: FakeRest): Client {
  return new Client({
    apiKey: KEY,
    baseUrl: "http://cetra.test",
    fetch: rest.fetch,
    sleep: rest.sleep,
  });
}

afterEach(() => {
  vi.unstubAllEnvs();
});

describe("Client", () => {
  it("clientNeedsAKey", () => {
    vi.stubEnv("CETRA_API_KEY", "");
    expect(() => new Client()).toThrow(ConfigError);
    expect(() => new Client()).toThrow("apiKey: required");
  });

  it("clientReprHidesTheKey", () => {
    const client = clientFor(new FakeRest(() => ({ status: 200, json: {} })));
    expect(JSON.stringify(client)).not.toContain(KEY);
    expect(String(client)).not.toContain(KEY);
  });

  it("requestSendsTheBearerAndJson", async () => {
    const rest = new FakeRest(inOrder({ status: 201, json: {} }));
    await clientFor(rest).request("POST", "/datasets", "/datasets", {
      body: { name: "faq" },
      retry: false,
    });
    const [request] = rest.requests;
    expect(request?.url).toBe(`${BASE}/datasets`);
    expect(request?.headers.Authorization).toBe(`Bearer ${KEY}`);
    expect(request?.headers["Content-Type"]).toBe("application/json");
    expect(request?.body).toBe('{"name":"faq"}');
  });

  it("retriesFiveHundredThenSucceeds", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 503 },
        { status: 429, headers: { "Retry-After": "2" } },
        { status: 200, json: { ok: true } },
      ),
    );
    const result = await clientFor(rest).request<{ ok: boolean }>("GET", "/datasets", "/datasets");
    expect(result).toEqual({ ok: true });
    expect(rest.requests).toHaveLength(3);
    expect(rest.sleeps).toEqual([1_000, 2_000]);
  });

  it("givesUpAfterThreeRetries", async () => {
    const rest = new FakeRest(
      inOrder({ status: 500 }, { status: 500 }, { status: 500 }, { status: 500 }),
    );
    const error = await clientFor(rest)
      .request("GET", "/datasets", "/datasets")
      .catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({ status: 500 });
    expect(rest.requests).toHaveLength(4);
    expect(rest.sleeps).toEqual([1_000, 2_000, 4_000]);
  });

  it("createIsNotRetried", async () => {
    const rest = new FakeRest(inOrder({ status: 503 }));
    await expect(clientFor(rest).datasets.create("faq")).rejects.toMatchObject({ status: 503 });
    expect(rest.requests).toHaveLength(1);
    expect(rest.sleeps).toEqual([]);
  });

  it("createNetworkFailureIsNotRetried", async () => {
    const rest = new FakeRest(inOrder(new TypeError("fetch failed")));
    await expect(clientFor(rest).datasets.create("faq")).rejects.toMatchObject({
      status: 0,
      code: "network",
    });
    expect(rest.requests).toHaveLength(1);
    expect(rest.sleeps).toEqual([]);
  });

  it("addItemsIsNotRetried", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 201, json: { id: "d1", name: "faq", current_version: 0, item_count: 0 } },
        { status: 503 },
      ),
    );
    const dataset = await clientFor(rest).datasets.create("faq");
    await expect(dataset.addItems([{ input: 1 }])).rejects.toMatchObject({ status: 503 });
    expect(rest.requests.filter((request) => request.method === "POST")).toHaveLength(2);
    expect(rest.sleeps).toEqual([]);
  });

  it("problemBecomesApiError", async () => {
    const rest = new FakeRest(
      inOrder({
        status: 409,
        headers: { "Content-Type": "application/problem+json" },
        json: { code: "conflict", detail: "a dataset with this name exists", errors: [] },
      }),
    );
    const error = await clientFor(rest)
      .request("POST", "/datasets", "/datasets", { body: {}, retry: false })
      .catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({ status: 409, code: "conflict" });
    expect((error as Error).message).toBe(
      "POST /datasets: 409 conflict: a dataset with this name exists",
    );
  });

  it("402IsNotAPause", async () => {
    const rest = new FakeRest(inOrder({ status: 402, headers: { "Retry-After": "5" } }));
    await expect(clientFor(rest).request("GET", "/datasets", "/datasets")).rejects.toMatchObject({
      status: 402,
    });
    expect(rest.requests).toHaveLength(1);
    expect(rest.sleeps).toEqual([]);
  });

  it("networkErrorIsRetriedThenRaised", async () => {
    const failures = [
      new TypeError("fetch failed"),
      new TypeError("fetch failed"),
      new TypeError("fetch failed"),
      new TypeError("fetch failed"),
    ];
    const rest = new FakeRest(inOrder(...failures));
    await expect(clientFor(rest).request("GET", "/datasets", "/datasets")).rejects.toMatchObject({
      status: 0,
      code: "network",
    });
    expect(rest.requests).toHaveLength(4);
    expect(rest.sleeps).toEqual([1_000, 2_000, 4_000]);
  });
});
