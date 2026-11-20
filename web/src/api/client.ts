import { MutationCache, QueryClient, type QueryKey } from "@tanstack/react-query";
import createClient from "openapi-fetch";
import createQueryHooks from "openapi-react-query";
import { problemOf } from "./problem";
import type { paths } from "./schema.gen";
import { unauthorizedMiddleware } from "./session";

export const api = createClient<paths>({
  baseUrl: window.location.origin,
  credentials: "same-origin",
  // Read fetch at call time. The client is created at import, before tests install MSW.
  fetch: (request: Request) => globalThis.fetch(request),
});

api.use(unauthorizedMiddleware);

export const $api = createQueryHooks(api);

/** Up to 2 retries. A problem response with status 400–499 is not retried. */
export function retryQuery(failureCount: number, error: unknown): boolean {
  const problem = problemOf(error);
  if (problem && problem.status >= 400 && problem.status <= 499) {
    return false;
  }
  return failureCount < 2;
}

const LIST_PATHS: ReadonlySet<string> = new Set([
  "/api/v1/projects/{project}/traces",
  "/api/v1/projects/{project}/spans",
  "/api/v1/projects/{project}/sessions",
  "/api/v1/projects/{project}/users",
]);

/** A list query of Traces, Spans, Sessions or Users (D-C-23). */
export function isListQuery(query: { queryKey: QueryKey }): boolean {
  return query.queryKey[0] === "get" && LIST_PATHS.has(String(query.queryKey[1]));
}

export function createAppQueryClient(): QueryClient {
  const client: QueryClient = new QueryClient({
    // A write marks the lists stale without refetching the one on screen; the next mount
    // refetches it in the background.
    mutationCache: new MutationCache({
      onSuccess: () => {
        void client.invalidateQueries({ predicate: isListQuery, refetchType: "none" });
      },
    }),
    defaultOptions: {
      queries: {
        staleTime: 30_000,
        refetchOnWindowFocus: false,
        retry: retryQuery,
      },
    },
  });
  return client;
}
