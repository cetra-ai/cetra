import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { FlaskConical, Loader2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { compareInit } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { CompareTable, ExperimentColumnHeader } from "@/components/experiments/compare-table";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  compareIdList,
  DEFAULT_THRESHOLD,
  deltaDirection,
  deltaText,
  parseCompareSearch,
  scoreSummaryText,
} from "@/lib/experiments";
import { formatCost, formatDuration } from "@/lib/format";

/** The threshold is written to the URL this long after the last keystroke. */
const THRESHOLD_DEBOUNCE_MS = 300;
/** The largest threshold the server accepts (D-J-13). */
const MAX_THRESHOLD = 1000;

export const Route = createFileRoute("/$org/$project/experiments/compare")({
  validateSearch: (search) => parseCompareSearch(search),
  component: ComparePage,
});

function ComparePage() {
  const { org, project } = useProject();
  const { ids } = Route.useSearch();
  if (compareIdList(ids).length < 2) {
    return (
      <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 px-4 py-6">
        <EmptyState
          icon={FlaskConical}
          title="Pick experiments to compare"
          description="Select two to four experiments of the same dataset on the Experiments page."
        />
        <div className="flex justify-center">
          <Button type="button" variant="outline" asChild>
            <Link to="/$org/$project/experiments" params={{ org, project }} search={{}}>
              Go to Experiments
            </Link>
          </Button>
        </div>
      </div>
    );
  }
  return <CompareView />;
}

function CompareView() {
  const { org, project } = useProject();
  const search = Route.useSearch();
  const navigate = useNavigate();
  const threshold = search.threshold ?? DEFAULT_THRESHOLD;
  const regressionsOnly = search.regressions === 1;
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments/compare",
    compareInit(org, project, compareIdList(search.ids), threshold, regressionsOnly),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
    },
  );

  const backLink = (
    <Link
      to="/$org/$project/experiments"
      params={{ org, project }}
      search={{}}
      className="w-fit rounded-sm text-xs text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
    >
      Experiments
    </Link>
  );

  // With no page loaded, a refused comparison (400) and a hidden experiment (404) are shown on the
  // page and anything else goes to the error boundary. After a page, a failed "Load more" is shown
  // next to the button.
  const hasPages = query.data !== undefined;
  const failure = hasPages ? undefined : (query.error ?? undefined);
  const nextPageFailure = hasPages ? (query.error ?? undefined) : undefined;
  const failureStatus = problemOf(failure)?.status;
  if (failure !== undefined && failureStatus !== 400 && failureStatus !== 404) {
    throw failure;
  }
  const failed = failure !== undefined;

  // Every page repeats the summaries, deltas and warnings, so the first one holds them.
  const first = query.data?.pages[0];
  const experiments = first?.experiments ?? [];
  const rows = query.data?.pages.flatMap((page) => page.rows) ?? [];

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      {backLink}
      <h1 className="text-lg font-medium">Compare experiments</h1>

      {failed ? (
        <div role="alert" className="rounded-md border border-status-error px-3 py-2 text-sm">
          {failureStatus === 404
            ? "One of these experiments was not found. It may have been deleted or be outside your plan's data window."
            : problemMessage(failure, "These experiments cannot be compared")}
        </div>
      ) : null}

      {first?.warnings.map((warning) => (
        <p
          key={warning}
          role="status"
          className="rounded-md border border-amber-500 px-3 py-2 text-sm text-amber-700 dark:text-amber-400"
        >
          {warning}
        </p>
      ))}

      {failed ? null : (
        <div className="flex flex-wrap items-end gap-6">
          <ThresholdInput threshold={threshold} />
          <div className="flex items-center gap-2">
            <Switch
              id="regressions-only"
              checked={regressionsOnly}
              onCheckedChange={(checked) => {
                void navigate({
                  to: "/$org/$project/experiments/compare",
                  params: { org, project },
                  search: (prev) => ({
                    ...prev,
                    ids: prev.ids ?? "",
                    regressions: checked ? 1 : undefined,
                  }),
                });
              }}
            />
            <Label htmlFor="regressions-only">Regressions only</Label>
          </div>
        </div>
      )}

      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : failed ? null : (
        <>
          <ScoreTable experiments={experiments} deltas={first?.deltas ?? []} />
          <section className="flex flex-col gap-3">
            {rows.length > 0 ? (
              <CompareTable rows={rows} experiments={experiments} />
            ) : query.hasNextPage ? null : (
              <p className="text-sm text-muted-foreground">
                {regressionsOnly ? "No regressions at this threshold" : "No items to compare"}
              </p>
            )}
            {nextPageFailure === undefined ? null : (
              <p role="alert" className="text-sm text-status-error">
                {problemMessage(nextPageFailure, "The next page could not be loaded")}
              </p>
            )}
            {query.hasNextPage ? (
              <Button
                type="button"
                variant="outline"
                className="w-fit"
                disabled={query.isFetchingNextPage}
                onClick={() => {
                  void query.fetchNextPage();
                }}
              >
                {query.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
                Load more
              </Button>
            ) : null}
          </section>
        </>
      )}
    </div>
  );
}

/** The draft as a threshold: a number in [0, 1000], or undefined when empty, not a number or out of range. */
function draftThreshold(draft: string): number | undefined {
  if (draft.trim() === "") {
    return undefined;
  }
  const value = Number(draft);
  return Number.isFinite(value) && value >= 0 && value <= MAX_THRESHOLD ? value : undefined;
}

/**
 * The regression threshold: written to `?threshold` after the typing pauses. When the URL changes
 * the draft takes the new value, unless the input has focus (the user is typing).
 */
function ThresholdInput({ threshold }: { threshold: number }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const [draft, setDraft] = useState(String(threshold));
  const inputRef = useRef<HTMLInputElement>(null);
  const invalid = draft.trim() !== "" && draftThreshold(draft) === undefined;

  useEffect(() => {
    if (document.activeElement !== inputRef.current) {
      setDraft(String(threshold));
    }
  }, [threshold]);

  useEffect(() => {
    const value = draftThreshold(draft);
    if (value === undefined || value === threshold) {
      return;
    }
    const timer = window.setTimeout(() => {
      void navigate({
        to: "/$org/$project/experiments/compare",
        params: { org, project },
        search: (prev) => ({ ...prev, ids: prev.ids ?? "", threshold: value }),
      });
    }, THRESHOLD_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [draft, threshold, navigate, org, project]);

  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor="regression-threshold">Regression threshold</Label>
      <Input
        ref={inputRef}
        id="regression-threshold"
        type="number"
        aria-label="Regression threshold"
        aria-invalid={invalid || undefined}
        aria-describedby={
          invalid
            ? "regression-threshold-help regression-threshold-range"
            : "regression-threshold-help"
        }
        className="w-32"
        step={0.05}
        min={0}
        max={MAX_THRESHOLD}
        value={draft}
        onChange={(event) => {
          setDraft(event.target.value);
        }}
      />
      <p id="regression-threshold-help" className="max-w-md text-xs text-muted-foreground">
        A score that drops by more than this, or a new error, marks a regression. Higher scores are
        better.
      </p>
      {invalid ? (
        <p id="regression-threshold-range" className="text-xs text-status-error">
          Between 0 and 1000
        </p>
      ) : null}
    </div>
  );
}

/** Score names in the baseline's order, then the names only the other experiments have. */
function scoreNames(experiments: Schema<"ExperimentSummary">[]): string[] {
  const names: string[] = [];
  for (const experiment of experiments) {
    for (const score of experiment.scores) {
      if (!names.includes(score.name)) {
        names.push(score.name);
      }
    }
  }
  return names;
}

const DIRECTION_CLASS = {
  better: "text-status-ok",
  worse: "text-status-error",
  same: "text-muted-foreground",
} as const;

/** The summary: one row per score, one column per experiment, deltas of the candidates. */
function ScoreTable({
  experiments,
  deltas,
}: {
  experiments: Schema<"ExperimentSummary">[];
  deltas: Schema<"ScoreDeltas">[];
}) {
  if (experiments.length === 0) {
    return null;
  }
  const names = scoreNames(experiments);
  return (
    <Table aria-label="Scores">
      <TableHeader>
        <TableRow>
          <TableHead>Score</TableHead>
          {experiments.map((summary, index) => (
            <TableHead key={summary.experiment.id}>
              <ExperimentColumnHeader experiment={summary.experiment} baseline={index === 0} />
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {names.map((name) => {
          const dataType = experiments
            .flatMap((experiment) => experiment.scores)
            .find((score) => score.name === name)?.data_type;
          return (
            <TableRow key={name}>
              <TableCell>{name}</TableCell>
              {experiments.map((summary, index) => {
                const score = summary.scores.find((candidate) => candidate.name === name);
                if (score === undefined) {
                  return <TableCell key={summary.experiment.id}>—</TableCell>;
                }
                const delta =
                  index === 0
                    ? undefined
                    : deltas.find((entry) => entry.experiment_id === summary.experiment.id)?.scores[
                        name
                      ];
                const text = scoreSummaryText(score);
                return (
                  <TableCell key={summary.experiment.id} className="tabular-nums">
                    {delta === undefined || dataType === undefined ? (
                      text
                    ) : (
                      <span className="flex flex-wrap items-center gap-2">
                        <span>{text}</span>
                        <Badge
                          variant="outline"
                          className={DIRECTION_CLASS[deltaDirection(delta.delta, dataType)]}
                        >
                          {deltaText(delta.delta, dataType)}
                          <span className="sr-only"> versus baseline</span>
                        </Badge>
                        <span className="text-xs text-muted-foreground">
                          {`n = ${delta.n_compared}`}
                        </span>
                      </span>
                    )}
                  </TableCell>
                );
              })}
            </TableRow>
          );
        })}
        <TableRow>
          <TableCell>Errors</TableCell>
          {experiments.map((summary) => (
            <TableCell key={summary.experiment.id} className="tabular-nums">
              {String(summary.error_count)}
            </TableCell>
          ))}
        </TableRow>
        <TableRow>
          <TableCell>Latency (mean)</TableCell>
          {experiments.map((summary) => (
            <TableCell key={summary.experiment.id} className="tabular-nums">
              {summary.latency_mean_ns === undefined
                ? "—"
                : formatDuration(summary.latency_mean_ns)}
            </TableCell>
          ))}
        </TableRow>
        <TableRow>
          <TableCell>Cost</TableCell>
          {experiments.map((summary) => (
            <TableCell key={summary.experiment.id} className="tabular-nums">
              {formatCost(summary.cost_total)}
            </TableCell>
          ))}
        </TableRow>
      </TableBody>
    </Table>
  );
}
