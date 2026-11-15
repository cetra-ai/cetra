import { QueryClient } from "@tanstack/react-query";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { savedQueriesQuery } from "./queries";

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
