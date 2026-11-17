import { keepPreviousData } from "@tanstack/react-query";
import { useMemo } from "react";
import { $api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";
import { LazyMount } from "@/components/charts/lazy-mount";
import { formFromRule, previewParams, previewSeries } from "@/lib/alerts";
import { seriesByMetric } from "@/lib/metrics";

const NS_PER_MS = 1_000_000;
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
  const series = previewSeries(formFromRule(rule));
  const window = rule.condition.window;
  const params = useMemo(() => previewParams(window, Date.now()), [window]);
  const filter = rule.condition.filter?.trim();
  const query = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/metrics",
    {
      params: {
        path: { project: projectRef(org, project) },
        query: {
          from: params.from,
          to: params.to,
          bucket: params.bucket,
          ...(filter ? { filter } : {}),
        },
      },
    },
    {
      staleTime: 60_000,
      placeholderData: keepPreviousData,
      enabled: series !== undefined,
    },
  );

  if (series === undefined) {
    return <Dash />;
  }
  if (query.isPending) {
    return <span aria-hidden="true" className="block" style={{ width: WIDTH, height: HEIGHT }} />;
  }
  if (query.error || !query.data) {
    return <Dash />;
  }
  const entry = seriesByMetric(query.data)
    .get(series.metric)
    ?.find((item) => (item.group ?? undefined) === series.group);
  const values =
    entry?.points.map((point) =>
      point.value === undefined || series.metric !== "latency_p95"
        ? point.value
        : point.value / NS_PER_MS,
    ) ?? [];
  const defined = values.filter((value): value is number => value !== undefined);
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
    .flatMap((value, index) => (value === undefined ? [] : [`${index * step},${y(value)}`]))
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

function Dash() {
  return <span className="text-muted-foreground">—</span>;
}
