import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { delay, HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import { server } from "@/mocks/node";
import { projectRef } from "./project-ref";
import {
  type FacetAsk,
  facetQuery,
  latestFacetAnswer,
  savedQueriesQuery,
  useFacetQuery,
} from "./queries";
import type { Schema } from "./types";

function client(): QueryClient {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

it("stops when the server repeats a page cursor", async () => {
  let calls = 0;
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () => {
      calls += 1;
      return HttpResponse.json({ data: [], next_cursor: "same" });
    }),
  );
  await expect(client().fetchQuery(savedQueriesQuery("acme", "support-bot"))).rejects.toThrow(
    "repeated a page cursor",
  );
  expect(calls).toBe(2);
});

it("stops after the page cap when every cursor is new", async () => {
  let calls = 0;
  server.use(
    http.get("*/api/v1/projects/:project/saved-views", () => {
      calls += 1;
      return HttpResponse.json({ data: [], next_cursor: `page-${calls}` });
    }),
  );
  await expect(client().fetchQuery(savedQueriesQuery("acme", "support-bot"))).rejects.toThrow(
    "too many pages",
  );
  expect(calls).toBe(1_000);
}, 30_000);

describe("facet answers from an earlier visit", () => {
  const ask = (from: string, to: string, more: Partial<FacetAsk> = {}): FacetAsk => ({
    view: "traces",
    field: "environment",
    from,
    to,
    ...more,
  });
  const answer = (count: number): Schema<"Facet"> => ({
    kind: "values",
    total: count,
    missing: 0,
    values: [{ value: "production", count }],
    more: false,
  });
  const seed = (cache: QueryClient, asked: FacetAsk, data: Schema<"Facet">) =>
    cache.setQueryData(facetQuery("acme", "support-bot", asked).queryKey, data);
  const key = (asked: FacetAsk) => facetQuery("acme", "support-bot", asked).queryKey;

  it("uses the newest answer over a window of the same length", () => {
    const cache = client();
    vi.useFakeTimers({ toFake: ["Date"] });
    try {
      vi.setSystemTime(1_000);
      seed(cache, ask("2026-11-18T10:00:00.000Z", "2026-11-18T11:00:00.000Z"), answer(1));
      vi.setSystemTime(2_000);
      seed(cache, ask("2026-11-18T10:00:20.000Z", "2026-11-18T11:00:20.000Z"), answer(2));
    } finally {
      vi.useRealTimers();
    }
    const found = latestFacetAnswer(
      cache,
      key(ask("2026-11-18T10:00:40.000Z", "2026-11-18T11:00:40.000Z")),
    );
    expect(found?.total).toBe(2);
  });

  it("facetQuery keys leave out empty parts", () => {
    const ref = projectRef("acme", "support-bot");
    expect(
      facetQuery("acme", "support-bot", {
        view: "traces",
        field: "status",
        from: "2026-11-18T10:00:00.000Z",
        to: "2026-11-18T11:00:00.000Z",
        filter: "",
        search: "",
        includeEval: false,
        q: "",
        limit: 10,
      }).queryKey,
    ).toEqual([
      "facets",
      ref,
      { view: "traces", from: "2026-11-18T10:00:00.000Z", to: "2026-11-18T11:00:00.000Z" },
      { field: "status", limit: 10 },
    ]);
  });

  it("ignores a window of another length", () => {
    const cache = client();
    seed(cache, ask("2026-11-18T09:00:00.000Z", "2026-11-18T11:00:00.000Z"), answer(1));
    expect(
      latestFacetAnswer(cache, key(ask("2026-11-18T10:00:40.000Z", "2026-11-18T11:00:40.000Z"))),
    ).toBeUndefined();
  });

  it("ignores another field, filter, view or project", () => {
    const cache = client();
    const from = "2026-11-18T10:00:00.000Z";
    const to = "2026-11-18T11:00:00.000Z";
    seed(cache, ask(from, to, { field: "name" }), answer(1));
    seed(cache, ask(from, to, { filter: "status = 'error'" }), answer(2));
    seed(cache, ask(from, to, { view: "spans" }), answer(3));
    cache.setQueryData(facetQuery("acme", "other", ask(from, to)).queryKey, answer(4));
    expect(
      latestFacetAnswer(cache, key(ask("2026-11-18T10:01:00.000Z", "2026-11-18T11:01:00.000Z"))),
    ).toBeUndefined();
  });

  it("shows the earlier answer while the new window loads", async () => {
    const cache = client();
    seed(cache, ask("2026-11-18T10:00:00.000Z", "2026-11-18T11:00:00.000Z"), answer(1));
    server.use(
      http.post("*/api/v1/projects/:project/facets", async ({ request }) => {
        const body = (await request.json()) as Schema<"FacetsRequest">;
        await delay(50);
        return HttpResponse.json({
          view: body.view,
          results: body.fields.map((field) => ({ field: field.field, facet: answer(7) })),
        });
      }),
    );
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={cache}>{children}</QueryClientProvider>
    );
    const { result } = renderHook(
      () =>
        useFacetQuery(
          "acme",
          "support-bot",
          ask("2026-11-18T10:00:30.000Z", "2026-11-18T11:00:30.000Z"),
        ),
      { wrapper },
    );
    expect(result.current.isPlaceholderData).toBe(true);
    expect(result.current.data?.total).toBe(1);
    await waitFor(() => expect(result.current.data?.total).toBe(7));
    expect(result.current.isPlaceholderData).toBe(false);
  });
});
