import { describe, expect, it } from "vitest";
import { createAppQueryClient, retryQuery } from "./client";

it("a successful mutation marks the list queries stale", async () => {
  const client = createAppQueryClient();
  const listKey = ["get", "/api/v1/projects/{project}/traces", {}];
  const otherKey = ["get", "/api/v1/projects/{project}/datasets", {}];
  client.setQueryData(listKey, { data: [], next_cursor: null });
  client.setQueryData(otherKey, { data: [] });
  await client
    .getMutationCache()
    .build(client, { mutationFn: async () => 1 })
    .execute(undefined);
  expect(client.getQueryState(listKey)?.isInvalidated).toBe(true);
  expect(client.getQueryState(otherKey)?.isInvalidated).toBe(false);
  client.clear();
});

describe("retryQuery", () => {
  it("retries twice, and never retries a 4xx problem", () => {
    const network = new Error("offline");
    expect(retryQuery(0, network)).toBe(true);
    expect(retryQuery(1, network)).toBe(true);
    expect(retryQuery(2, network)).toBe(false);
    expect(
      retryQuery(0, {
        type: "about:blank",
        title: "Unauthorized",
        status: 401,
        code: "unauthorized",
      }),
    ).toBe(false);
    expect(
      retryQuery(0, { type: "about:blank", title: "Server error", status: 500, code: "internal" }),
    ).toBe(true);

    const retry = createAppQueryClient().getDefaultOptions().queries?.retry;
    expect(retry).toBe(retryQuery);
  });
});
