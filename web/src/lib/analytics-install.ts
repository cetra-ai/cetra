import type { QueryClient } from "@tanstack/react-query";
import { authConfigQuery } from "@/api/auth";
import { type AnalyticsContext, type Sender, startAnalytics, track } from "@/lib/analytics";
import { isPublicPath } from "@/lib/redirect";
import type { AppRouter } from "@/router";

/** The leaf route's id (`/$org/$project/traces/`): the page template, never the URL. */
export function pageRoute(router: AppRouter): string | undefined {
  const leaf = router.state.matches.at(-1);
  if (!leaf || leaf.routeId === "__root__" || isPublicPath(router.state.location.pathname)) {
    return undefined;
  }
  return leaf.routeId;
}

/** The leaf route's `org` and `project` parameters, as in the URL. */
export function pageContext(router: AppRouter): AnalyticsContext {
  const params = (router.state.matches.at(-1)?.params ?? {}) as Record<string, unknown>;
  return {
    ...(typeof params.org === "string" ? { org: params.org } : {}),
    ...(typeof params.project === "string" ? { project: params.project } : {}),
  };
}

/**
 * Turns analytics on when the server says so: a page view now (if the router has resolved a
 * page) and on every path change. Returns `uninstall`. Does nothing when `analytics` is off
 * or the config cannot be read.
 */
export async function installAnalytics(
  router: AppRouter,
  queryClient: QueryClient,
  options: { send?: Sender; flushMs?: number } = {},
): Promise<() => void> {
  let enabled = false;
  try {
    enabled = (await queryClient.fetchQuery(authConfigQuery)).analytics;
  } catch {
    return () => {};
  }
  if (!enabled) {
    return () => {};
  }
  const stop = startAnalytics({ ...options, context: () => pageContext(router) });
  const viewed = () => {
    const route = pageRoute(router);
    if (route !== undefined) {
      track({ name: "page.viewed", properties: { route } });
    }
  };
  const unsubscribe = router.subscribe("onResolved", (event) => {
    if (event.fromLocation?.pathname === event.toLocation.pathname) {
      return;
    }
    viewed();
  });
  if (router.state.resolvedLocation !== undefined) {
    viewed();
  }
  return () => {
    unsubscribe();
    stop();
  };
}
