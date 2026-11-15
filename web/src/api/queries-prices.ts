import { $api } from "./client";
import { projectRef } from "./project-ref";

export function orgPricesInit(org: string) {
  return { params: { path: { org } } };
}

export function orgPricesKey(org: string) {
  return $api.queryOptions("get", "/api/v1/orgs/{org}/models", orgPricesInit(org)).queryKey;
}

export function projectPricesInit(
  org: string,
  project: string,
  scope: "project" | "org" | "builtin",
  q: string,
) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { scope, limit: 100, ...(q ? { q } : {}) },
    },
  };
}

/**
 * The key prefix of every project price list and resolution (all projects, all scopes), so one
 * invalidation refreshes them; `org-members.tsx` invalidates a path prefix the same way.
 */
export const PROJECT_PRICES_PREFIX = ["get", "/api/v1/projects/{project}/models"] as const;
export const PROJECT_RESOLVE_PREFIX = ["get", "/api/v1/projects/{project}/models/resolve"] as const;
