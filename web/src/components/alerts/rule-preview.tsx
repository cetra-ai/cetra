import { useQuery } from "@tanstack/react-query";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { CartesianGrid, ComposedChart, Line, ReferenceLine, XAxis, YAxis } from "recharts";
import { widgetQuery } from "@/api/dashboards";
import { problemOf } from "@/api/problem";
import { type IssueAlertScope, issueAlertPreviewQuery } from "@/api/queries-alerts";
import { AXIS_TICK, axisStyle, formatTick } from "@/components/charts/plot-helpers";
import { filterMessage } from "@/components/filter-bar/logic";
import { FormError } from "@/components/form/form-error";
import { ChartContainer } from "@/components/ui/chart";
import { Skeleton } from "@/components/ui/skeleton";
import {
  conditionFromForm,
  formatAlertValue,
  issuePreviewRows,
  issuePreviewScope,
  metricLabel,
  previewCondition,
  previewParams,
  previewQuery,
  previewRows,
  type RuleFormValues,
  thresholdLabelPosition,
} from "@/lib/alerts";
import type { WidgetQuery } from "@/lib/dashboards/types";
import type { ChartRow } from "@/lib/metrics";
import { useTimeZone } from "@/lib/settings";

const RANGE_LABEL: Record<RuleFormValues["window"], string> = {
  "1m": "24 hours",
  "5m": "24 hours",
  "15m": "24 hours",
  "1h": "24 hours",
  "6h": "3 days",
  "24h": "14 days",
};

const METRIC_CAPTION =
  "Each point is the metric over one window, as the rule sees it. Minimum samples are not shown.";
const ISSUE_CAPTION =
  "Each point is the occurrences over one window, as the rule counts them now: merged issues count with their target.";

/** The metric over the rule's window-wide buckets, with the threshold as a line (D-K-13). Issue
 *  occurrences are counted by the server per bucket (D-K-22). */
export function RulePreview(props: { org: string; project: string; values: RuleFormValues }) {
  const { org, project, values } = props;
  const issue = issuePreviewScope(values);
  if (issue !== undefined) {
    return issue.kind === "missing" ? (
      <PreviewFrame
        caption={ISSUE_CAPTION}
        body={<p className="text-body text-muted-foreground">{issue.message}</p>}
      />
    ) : (
      <IssuePreviewBody
        org={org}
        project={project}
        values={values}
        scope={issue.scope}
        window={issue.window}
      />
    );
  }
  const preview = previewQuery(values);
  if (preview === undefined) {
    return (
      <div className="rounded-md border border-dashed p-4 text-body text-muted-foreground">
        No preview for LLM span latency. The dashboard shows it per model.
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
    body = <PreviewChart rows={rows} values={values} />;
  }

  return <PreviewFrame body={body} caption={METRIC_CAPTION} />;
}

/** The preview of an issue occurrence rule: the server's count per bucket (D-K-22). Zero counts
 *  are values, so an empty range draws zeros rather than "No data in this range.". */
function IssuePreviewBody({
  org,
  project,
  values,
  scope,
  window,
}: {
  org: string;
  project: string;
  values: RuleFormValues;
  scope: IssueAlertScope;
  window: RuleFormValues["window"];
}) {
  const query = useQuery({
    ...issueAlertPreviewQuery(org, project, scope, window),
    staleTime: 60_000,
  });

  let body: ReactNode;
  if (query.isPending) {
    body = <Skeleton className="h-40 w-full" />;
  } else if (query.error) {
    body =
      problemOf(query.error)?.status === 404 ? (
        <p className="text-body text-muted-foreground">The issue or source no longer exists.</p>
      ) : (
        <p className="text-body text-muted-foreground">The preview could not be loaded.</p>
      );
  } else {
    body = <PreviewChart rows={issuePreviewRows(query.data)} values={values} />;
  }

  return <PreviewFrame body={body} caption={ISSUE_CAPTION} />;
}

function PreviewFrame({ body, caption }: { body: ReactNode; caption: string }) {
  return (
    <div className="flex flex-col gap-2">
      <h2 className="text-body font-medium">Preview</h2>
      {body}
      <p className="text-caption text-muted-foreground">{caption}</p>
    </div>
  );
}

function PreviewChart({ rows, values }: { rows: ChartRow[]; values: RuleFormValues }) {
  const zone = useTimeZone();
  const built = conditionFromForm(values);
  const threshold = built.ok ? built.condition.threshold : undefined;
  const condition = previewCondition(values);
  const style = axisStyle(rows);

  return (
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
            tick={AXIS_TICK}
            padding={{ left: 8, right: 24 }}
            tickFormatter={(value) => formatTick(value, zone, style)}
            minTickGap={28}
            axisLine={false}
            tickLine={false}
          />
          <YAxis
            tick={AXIS_TICK}
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
              label={{
                value: "Threshold",
                position: thresholdLabelPosition(values.comparator),
                offset: 6,
                fontSize: 11,
                fill: "var(--muted-foreground)",
              }}
            />
          )}
        </ComposedChart>
      </ChartContainer>
    </figure>
  );
}
