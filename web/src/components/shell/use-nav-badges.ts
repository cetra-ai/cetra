import { useQuery } from "@tanstack/react-query";
import { $api } from "@/api/client";
import { alertRulesQuery } from "@/api/queries-alerts";
import { experimentsInit } from "@/api/queries-experiments";
import { onlineRulesInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";

export type NavBadge =
  | { kind: "count"; value: number; tone: "destructive" | "muted"; label: string }
  | { kind: "dot"; tone: "warning"; label: string };

export type NavBadgeHref =
  | "/$org/$project/alerts"
  | "/$org/$project/online-rules"
  | "/$org/$project/experiments";

/** The enabled rules that are firing, in the response's order. */
export function firingRules(rules: readonly Schema<"AlertRule">[]): Schema<"AlertRule">[] {
  return rules.filter((rule) => rule.enabled && rule.state === "firing");
}

/**
 * The live badge of each sidebar page that has one. Each badge reads the query its page makes, with
 * the same key and the same plain or infinite kind, so the page and the sidebar share one cache entry.
 * A failed or loading query gives no badge.
 */
export function useNavBadges(
  org: string,
  project: string,
): Partial<Record<NavBadgeHref, NavBadge>> {
  // Same as the alerts page's query (plain, no cursor).
  const alerts = useQuery({ ...alertRulesQuery(org, project), refetchInterval: 60_000 });
  // Same arguments as the online rules page's infinite query.
  const onlineRules = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      refetchInterval: 60_000,
    },
  );
  // Same arguments as the experiments page's infinite query with no dataset filter.
  const experiments = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments",
    experimentsInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      refetchInterval: 30_000,
    },
  );

  const badges: Partial<Record<NavBadgeHref, NavBadge>> = {};

  const firing = firingRules(alerts.data?.data ?? []).length;
  if (firing > 0) {
    badges["/$org/$project/alerts"] = {
      kind: "count",
      value: firing,
      tone: "destructive",
      label: `${firing} alert ${firing === 1 ? "rule" : "rules"} firing`,
    };
  }

  const pausedOnErrors = (onlineRules.data?.pages.flatMap((page) => page.data) ?? []).filter(
    (rule) => rule.enabled && rule.paused_reason === "errors",
  ).length;
  if (pausedOnErrors > 0) {
    badges["/$org/$project/online-rules"] = {
      kind: "dot",
      tone: "warning",
      label:
        pausedOnErrors === 1
          ? "An online rule is paused on errors"
          : `${pausedOnErrors} online rules paused on errors`,
    };
  }

  const running = (experiments.data?.pages.flatMap((page) => page.data) ?? []).filter(
    (row) => row.status === "running",
  ).length;
  if (running > 0) {
    badges["/$org/$project/experiments"] = {
      kind: "count",
      value: running,
      tone: "muted",
      label: `${running} ${running === 1 ? "experiment" : "experiments"} running`,
    };
  }

  return badges;
}
