import { useQuery } from "@tanstack/react-query";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { CartesianGrid, ComposedChart, Line, ReferenceLine, XAxis, YAxis } from "recharts";
import { widgetQuery } from "@/api/dashboards";
import { problemOf } from "@/api/problem";
import { filterMessage } from "@/components/filter-bar/logic";
import { FormError } from "@/components/form/form-error";
import { ChartContainer } from "@/components/ui/chart";
import { Skeleton } from "@/components/ui/skeleton";
import {
  conditionFromForm,
  formatAlertValue,
  metricLabel,
  previewCondition,
  previewParams,
  previewQuery,
  previewRows,
  type RuleFormValues,
} from "@/lib/alerts";
import type { WidgetQuery } from "@/lib/dashboards/types";
import { formatTimestamp } from "@/lib/format";
import { useTimeZone } from "@/lib/settings";

const RANGE_LABEL: Record<RuleFormValues["window"], string> = {
  "1m": "24 hours",
  "5m": "24 hours",
  "15m": "24 hours",
  "1h": "24 hours",
  "6h": "3 days",
  "24h": "14 days",
};

/** The metric over the rule's window-wide buckets, with the threshold as a line (D-K-13). */
export function RulePreview(props: { org: string; project: string; values: RuleFormValues }) {
  const { values } = props;
  const preview = previewQuery(values);
  if (preview === undefined) {
    return (
      <div className="rounded-md border border-dashed p-4 text-body text-muted-foreground">
        {values.metric === "issue_occurrences"
          ? "No preview for issue occurrences."
          : "No preview for LLM span latency. The dashboard shows it per model."}
      </div>
    );
  }
  return <PreviewBody {...props} query={preview.query} divisor={preview.divisor} />;
}

function PreviewBody({
  org,
  project,
  values,
  query: widget,
  divisor,
}: {
  org: string;
  project: string;
  values: RuleFormValues;
  query: WidgetQuery;
  divisor: number;
}) {
  const zone = useTimeZone();
  const [filter, setFilter] = useState(values.filter.trim());
  useEffect(() => {
    const timer = setTimeout(() => {
      setFilter(values.filter.trim());
    }, 500);
    return () => {
      clearTimeout(timer);
    };
  }, [values.filter]);
  const params = useMemo(() => previewParams(values.window, Date.now()), [values.window]);
  const query = useQuery({
    ...widgetQuery(org, project, {
      from: params.from,
      to: params.to,
      shape: "series",
      bucket: params.bucket,
      query: widget,
      ...(filter ? { filter } : {}),
    }),
    staleTime: 60_000,
  });

  const rows = previewRows(widget, query.data, divisor);
  const hasValue = rows.some((row) => typeof row.all === "number");
  const built = conditionFromForm(values);
  const threshold = built.ok ? built.condition.threshold : undefined;
  const condition = previewCondition(values);

  let body: ReactNode;
  if (query.isPending) {
    body = <Skeleton className="h-40 w-full" />;
  } else if (query.error) {
    const problem = problemOf(query.error);
    body =
      problem?.code === "invalid_filter" ? (
        <FormError message={`The filter is invalid: ${filterMessage(problem)}`} />
      ) : (
        <p className="text-body text-muted-foreground">The preview could not be loaded.</p>
      );
  } else if (!hasValue) {
    body = <p className="text-body text-muted-foreground">No data in this range.</p>;
  } else {
    body = (
      <figure aria-label={`Preview of the metric over the last ${RANGE_LABEL[values.window]}`}>
        <ChartContainer
          config={{ all: { label: metricLabel(condition), color: "var(--chart-1)" } }}
          data-chart-container=""
          initialDimension={{ width: 640, height: 160 }}
          className="aspect-auto h-40 w-full"
        >
          <ComposedChart data={rows} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
            <CartesianGrid stroke="var(--border)" strokeOpacity={0.7} vertical={false} />
            <XAxis
              dataKey="time"
              tickFormatter={(ms) => formatTimestamp(new Date(ms).toISOString(), zone, "datetime")}
              minTickGap={28}
              axisLine={false}
              tickLine={false}
            />
            <YAxis
              tickFormatter={(value) =>
                formatAlertValue(condition, typeof value === "number" ? value : undefined)
              }
              width={72}
              axisLine={false}
              tickLine={false}
            />
            <Line
              dataKey="all"
              stroke="var(--chart-1)"
              strokeWidth={1.5}
              dot={false}
              connectNulls={false}
              isAnimationActive={false}
              type="linear"
            />
            {threshold === undefined ? null : (
              <ReferenceLine
                y={threshold}
                stroke="var(--destructive)"
                strokeDasharray="4 4"
                ifOverflow="extendDomain"
                label={{ value: "Threshold", position: "insideTopRight", fontSize: 11 }}
              />
            )}
          </ComposedChart>
        </ChartContainer>
      </figure>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <h2 className="text-body font-medium">Preview</h2>
      {body}
      <p className="text-caption text-muted-foreground">
        Each point is the metric over one window, as the rule sees it. Minimum samples are not
        shown.
      </p>
    </div>
  );
}
