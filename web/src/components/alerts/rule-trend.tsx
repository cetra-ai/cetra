import { useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { widgetQuery } from "@/api/dashboards";
import { type IssueAlertScope, issueAlertPreviewQuery } from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { LazyMount } from "@/components/charts/lazy-mount";
import {
  formFromRule,
  issuePreviewRows,
  issuePreviewScope,
  previewParams,
  previewQuery,
  previewRows,
} from "@/lib/alerts";
import type { WidgetQuery } from "@/lib/dashboards/types";

const WIDTH = 120;
const HEIGHT = 28;
const PAD = 2;

/**
 * The rule's metric over the same range the rule preview uses, with the threshold as a dashed
 * line. The chart fetches only once it is near the viewport. No data or an error shows a dash.
 */
export function RuleTrend({
  org,
  project,
  rule,
}: {
  org: string;
  project: string;
  rule: Schema<"AlertRule">;
}) {
  return (
    <LazyMount eager={false} minHeight={HEIGHT} fallback={null}>
      <TrendChart org={org} project={project} rule={rule} />
    </LazyMount>
  );
}

function TrendChart({
  org,
  project,
  rule,
}: {
  org: string;
  project: string;
  rule: Schema<"AlertRule">;
}) {
  const form = formFromRule(rule);
  const issue = issuePreviewScope(form);
  if (issue !== undefined) {
    return issue.kind === "ready" ? (
      <IssueTrend
        org={org}
        project={project}
        rule={rule}
        scope={issue.scope}
        window={issue.window}
      />
    ) : (
      <Dash />
    );
  }
  const preview = previewQuery(form);
  if (preview === undefined) {
    return <Dash />;
  }
  return (
    <Trend
      org={org}
      project={project}
      rule={rule}
      query={preview.query}
      divisor={preview.divisor}
    />
  );
}

function Trend({
  org,
  project,
  rule,
  query: widget,
  divisor,
}: {
  org: string;
  project: string;
  rule: Schema<"AlertRule">;
  query: WidgetQuery;
  divisor: number;
}) {
  const window = rule.condition.window;
  const params = useMemo(() => previewParams(window, Date.now()), [window]);
  const filter = rule.condition.filter?.trim();
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

  if (query.isPending) {
    return <Placeholder />;
  }
  if (query.error || !query.data) {
    return <Dash />;
  }
  return (
    <TrendLine
      rule={rule}
      values={previewRows(widget, query.data, divisor).map((row) => row.all ?? null)}
    />
  );
}

/** The occurrences of an issue occurrence rule per window, from the server (D-K-22). */
function IssueTrend({
  org,
  project,
  rule,
  scope,
  window,
}: {
  org: string;
  project: string;
  rule: Schema<"AlertRule">;
  scope: IssueAlertScope;
  window: Schema<"AlertWindow">;
}) {
  const query = useQuery({
    ...issueAlertPreviewQuery(org, project, scope, window),
    staleTime: 60_000,
  });

  if (query.isPending) {
    return <Placeholder />;
  }
  if (query.error || !query.data) {
    return <Dash />;
  }
  return (
    <TrendLine rule={rule} values={issuePreviewRows(query.data).map((row) => row.all ?? null)} />
  );
}

function TrendLine({ rule, values }: { rule: Schema<"AlertRule">; values: (number | null)[] }) {
  const window = rule.condition.window;
  const defined = values.filter((value): value is number => value !== null);
  if (defined.length === 0) {
    return <Dash />;
  }

  const threshold = rule.condition.threshold;
  const low = Math.min(threshold, ...defined);
  const high = Math.max(threshold, ...defined);
  const span = high - low || 1;
  const y = (value: number) => PAD + (1 - (value - low) / span) * (HEIGHT - 2 * PAD);
  const step = values.length > 1 ? WIDTH / (values.length - 1) : 0;
  const line = values
    .flatMap((value, index) => (value === null ? [] : [`${index * step},${y(value)}`]))
    .join(" ");

  return (
    <svg
      role="img"
      aria-label={`Trend of ${rule.name} over the last ${window}`}
      width={WIDTH}
      height={HEIGHT}
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      className="block"
    >
      <line
        x1={0}
        x2={WIDTH}
        y1={y(threshold)}
        y2={y(threshold)}
        strokeDasharray="3 3"
        strokeWidth={1}
        style={{ stroke: "var(--destructive)" }}
      />
      <polyline
        points={line}
        fill="none"
        strokeWidth={1.5}
        strokeLinejoin="round"
        style={{ stroke: "var(--chart-1)" }}
      />
    </svg>
  );
}

function Placeholder() {
  return <span aria-hidden="true" className="block" style={{ width: WIDTH, height: HEIGHT }} />;
}

function Dash() {
  return <span className="text-muted-foreground">—</span>;
}
