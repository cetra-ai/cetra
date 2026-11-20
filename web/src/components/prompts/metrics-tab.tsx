import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { promptMetricsQuery } from "@/api/queries-prompts";
import { useProject } from "@/components/shell/project-context";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { formatCount, formatPercent } from "@/lib/metrics";
import { METRICS_PRESETS, type MetricsPreset, metricsWindow, tracesSearchFor } from "@/lib/prompts";

const WINDOW_LABELS: Record<MetricsPreset, string> = {
  "24h": "24 hours",
  "7d": "7 days",
  "30d": "30 days",
};
const DEFAULT_PRESET: MetricsPreset = "7d";
const NO_LINKED_SPANS =
  "No linked spans in this window. Link spans with context(prompt=…) in the SDK.";

/** A latency in milliseconds as text; `—` when the server omits it. */
function latency(ms: number | undefined): string {
  return ms === undefined ? "—" : formatDuration(ms * 1_000_000);
}

/**
 * The Metrics tab: per version, the spans and traces linked to the prompt over a window, the error
 * rate, cost, latency, tokens and score means, and a link to the traces of that version.
 */
export function MetricsTab({ prompt }: { prompt: string }) {
  const { org, project } = useProject();
  const [preset, setPreset] = useState<MetricsPreset>(DEFAULT_PRESET);
  // Computed once per chosen window, so the query key does not change on every render.
  const bounds = useMemo(() => metricsWindow(preset, Date.now()), [preset]);
  const metrics = useQuery(promptMetricsQuery(org, project, prompt, bounds.from, bounds.to));

  if (metrics.isError) {
    throw metrics.error;
  }

  function chooseWindow(value: string) {
    const next = METRICS_PRESETS.find((item) => item === value);
    if (next !== undefined) {
      setPreset(next);
    }
  }

  const rows = metrics.data?.versions ?? [];
  const scoreNames = [
    ...new Set(rows.flatMap((row) => row.scores.map((score) => score.name))),
  ].sort();

  return (
    <div className="flex flex-col gap-4">
      <div className="flex justify-end">
        <Select value={preset} onValueChange={chooseWindow}>
          <SelectTrigger aria-label="Window" size="sm" className="w-fit text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {METRICS_PRESETS.map((item) => (
              <SelectItem key={item} value={item}>
                {WINDOW_LABELS[item]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {metrics.isPending ? <Skeleton className="h-24 w-full" /> : null}
      {metrics.data && rows.length === 0 ? (
        <p className="text-body text-muted-foreground">{NO_LINKED_SPANS}</p>
      ) : null}
      {rows.length > 0 ? (
        <Table aria-label="Metrics by version">
          <TableHeader>
            <TableRow>
              <TableHead>Version</TableHead>
              <TableHead>Spans</TableHead>
              <TableHead>Traces</TableHead>
              <TableHead>Error rate</TableHead>
              <TableHead>Avg cost</TableHead>
              <TableHead>p50</TableHead>
              <TableHead>p95</TableHead>
              <TableHead>Avg tokens in / out</TableHead>
              {scoreNames.map((name) => (
                <TableHead key={name}>{name}</TableHead>
              ))}
              <TableHead>
                <span className="sr-only">Links</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((row) => (
              <TableRow key={row.version}>
                <TableCell className="num font-medium">{`v${row.version}`}</TableCell>
                <TableCell className="num">{formatCount(row.span_count)}</TableCell>
                <TableCell className="num">{formatCount(row.trace_count)}</TableCell>
                <TableCell className="num">
                  {row.span_count === 0 ? "—" : formatPercent(row.error_count / row.span_count)}
                </TableCell>
                <TableCell className="num">{formatCost(row.cost_avg)}</TableCell>
                <TableCell className="num">{latency(row.latency_p50_ms)}</TableCell>
                <TableCell className="num">{latency(row.latency_p95_ms)}</TableCell>
                <TableCell className="num">
                  {`${formatTokens(row.input_tokens_avg)} / ${formatTokens(row.output_tokens_avg)}`}
                </TableCell>
                {scoreNames.map((name) => {
                  const score = row.scores.find((item) => item.name === name);
                  return (
                    <TableCell key={name} className="num">
                      {score === undefined ? (
                        "—"
                      ) : (
                        <>
                          {score.mean.toFixed(2)}{" "}
                          <span className="text-muted-foreground">{`n = ${score.count}`}</span>
                        </>
                      )}
                    </TableCell>
                  );
                })}
                <TableCell>
                  <Link
                    to="/$org/$project/traces"
                    params={{ org, project }}
                    search={tracesSearchFor(prompt, row.version, preset)}
                    className="text-body underline-offset-4 hover:underline"
                  >
                    Traces
                  </Link>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      ) : null}
    </div>
  );
}
