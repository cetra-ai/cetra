import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { widgetQuery } from "@/api/dashboards";
import type { Schema } from "@/api/types";
import type { WidgetContext } from "./widget";

/** The models of the range, most expensive first: the order of the model colours. */
const RANKING_LIMIT = 10;

/**
 * The cost ranking of the models in the dashboard's range and filter, as the top models list
 * ranks them. Every widget takes its model colours from this order. `undefined` while it loads,
 * and when no model is in range.
 */
export function useModelRanking(context: WidgetContext): {
  ranking: string[] | undefined;
  error: unknown;
  /** Epoch milliseconds of the answer on screen; undefined until the first one arrives. */
  updatedAt: number | undefined;
} {
  const query = useQuery(
    widgetQuery(context.org, context.project, {
      from: context.from,
      to: context.to,
      shape: "total",
      ...(context.filter ? { filter: context.filter } : {}),
      include_eval: context.includeEval,
      include_experiments: context.includeExperiments,
      query: {
        source: "spans",
        filter: "span_kind = 'llm' AND model IS NOT NULL",
        measures: [{ agg: "sum", field: "cost_total" }],
        breakdown: { field: "model", limit: RANKING_LIMIT, other: false },
      },
    }),
  );
  const ranking = useMemo(() => rankedModels(query.data), [query.data]);
  // Before the first answer there is no time to show.
  const updatedAt = query.dataUpdatedAt > 0 ? query.dataUpdatedAt : undefined;
  return { ranking, error: query.error, updatedAt };
}

function rankedModels(answer: Schema<"WidgetQueryResponse"> | undefined): string[] | undefined {
  if (answer?.shape !== "total") {
    return undefined;
  }
  const models = answer.rows.flatMap((row) =>
    row.group !== undefined && !row.other ? [row.group] : [],
  );
  return models.length > 0 ? models : undefined;
}
