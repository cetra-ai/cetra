import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { FlaskConical, Info, Loader2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { compareInit } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { CompareItemSheet } from "@/components/experiments/compare-item-view";
import {
  CompareTable,
  ExperimentColumnHeader,
  scoreNames,
} from "@/components/experiments/compare-table";
import { ScoreAverageBadge } from "@/components/score-badge";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import {
  type CompareOnly,
  compareIdList,
  DEFAULT_THRESHOLD,
  DIRECTION_CLASS,
  deltaDirection,
  deltaText,
  headlineText,
  headlineValue,
  metadataTags,
  parseCompareSearch,
  scoreSummaryText,
} from "@/lib/experiments";
import { formatCost, formatDuration, formatRelative } from "@/lib/format";

/** The threshold is written to the URL this long after the last keystroke. */
const THRESHOLD_DEBOUNCE_MS = 300;
/** The largest threshold the server accepts (D-J-13). */
const MAX_THRESHOLD = 1000;
const MINUS = "−";

/** The segmented control of `only`: undefined is every item. */
const ONLY_OPTIONS: { value: CompareOnly | undefined; label: string }[] = [
  { value: undefined, label: "All" },
  { value: "regressed", label: "Regressed" },
  { value: "improved", label: "Improved" },
  { value: "changed", label: "Changed" },
];

/** The message of an empty item list, per `only`. */
const EMPTY_MESSAGE: Record<CompareOnly, string> = {
  regressed: "No regressions at this threshold",
  improved: "No improvements at this threshold",
  changed: "No changes at this threshold",
};

export const Route = createFileRoute("/$org/$project/experiments/compare")({
  validateSearch: (search) => parseCompareSearch(search),
  component: ComparePage,
});

function ComparePage() {
  const { org, project } = useProject();
  const { ids } = Route.useSearch();
  if (compareIdList(ids).length < 2) {
    return (
      <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-6 px-4 py-6">
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
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/experiments/compare",
    compareInit(org, project, compareIdList(search.ids), threshold, search.only),
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
      className="w-fit rounded-sm text-body text-muted-foreground underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
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
  const selected =
    search.item === undefined ? undefined : rows.find((row) => row.dataset_item_id === search.item);

  function setOnly(only: CompareOnly | undefined) {
    void navigate({
      to: "/$org/$project/experiments/compare",
      params: { org, project },
      search: (prev) => ({ ...prev, ids: prev.ids ?? "", only }),
    });
  }

  function openItem(datasetItemId: string | undefined) {
    void navigate({
      to: "/$org/$project/experiments/compare",
      params: { org, project },
      search: (prev) => ({ ...prev, ids: prev.ids ?? "", item: datasetItemId }),
    });
  }

  // Up and down move to the neighbouring loaded item; past the ends nothing happens.
  function stepItem(delta: 1 | -1) {
    const index = rows.findIndex((row) => row.dataset_item_id === search.item);
    const next = rows[index + delta];
    if (index !== -1 && next !== undefined) {
      openItem(next.dataset_item_id);
    }
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <div className="flex min-h-9 shrink-0 flex-wrap items-center gap-2 border-b px-3 py-1">
        {backLink}
        <h2 className="text-ui font-medium">Compare experiments</h2>
        {failed ? null : (
          <>
            <fieldset
              aria-label="Show"
              className="m-0 flex min-w-0 flex-wrap items-center gap-1 border-0 p-0"
            >
              {ONLY_OPTIONS.map((option) => (
                <Button
                  key={option.label}
                  type="button"
                  size="xs"
                  variant={search.only === option.value ? "secondary" : "ghost"}
                  aria-pressed={search.only === option.value}
                  onClick={() => {
                    setOnly(option.value);
                  }}
                >
                  {option.label}
                </Button>
              ))}
            </fieldset>
            <div className="ml-auto">
              <ThresholdInput threshold={threshold} />
            </div>
          </>
        )}
      </div>

      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto px-3 py-3">
        {failed ? (
          <div role="alert" className="rounded-md border border-status-error px-3 py-2 text-ui">
            {failureStatus === 404
              ? "One of these experiments was not found. It may have been deleted or be outside your plan's data window."
              : problemMessage(failure, "These experiments cannot be compared")}
          </div>
        ) : null}

        {first?.warnings.map((warning) => (
          <p
            key={warning}
            role="status"
            className="rounded-md border border-amber-500 px-3 py-2 text-ui text-amber-700 dark:text-amber-400"
          >
            {warning}
          </p>
        ))}

        {query.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : failed ? null : (
          <>
            <CompareSummary
              experiments={experiments}
              deltas={first?.deltas ?? []}
              onOnly={setOnly}
            />
            <section className="flex flex-col gap-3">
              {rows.length > 0 ? (
                <CompareTable rows={rows} experiments={experiments} onOpenItem={openItem} />
              ) : query.hasNextPage ? null : (
                <p className="text-ui text-muted-foreground">
                  {search.only === undefined ? "No items to compare" : EMPTY_MESSAGE[search.only]}
                </p>
              )}
              {nextPageFailure === undefined ? null : (
                <p role="alert" className="text-ui text-status-error">
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

      <CompareItemSheet
        row={failed ? undefined : selected}
        experiments={experiments}
        onStep={stepItem}
        onClose={() => {
          openItem(undefined);
        }}
      />
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
 * the draft takes the new value, unless the input has focus (the user is typing). Its help text
 * is in a tooltip.
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
    <div className="flex items-center gap-1">
      <span className="text-body text-muted-foreground">Threshold</span>
      <Input
        ref={inputRef}
        id="regression-threshold"
        type="number"
        aria-label="Regression threshold"
        aria-invalid={invalid || undefined}
        aria-describedby={invalid ? "regression-threshold-range" : undefined}
        className="h-7 w-20 text-body tabular-nums"
        step={0.05}
        min={0}
        max={MAX_THRESHOLD}
        value={draft}
        onChange={(event) => {
          setDraft(event.target.value);
        }}
      />
      <Tooltip>
        <TooltipTrigger asChild>
          <Button type="button" variant="ghost" size="icon-xs" aria-label="About the threshold">
            <Info />
          </Button>
        </TooltipTrigger>
        <TooltipContent className="max-w-xs text-body">
          A score that drops by more than this, or a new error, marks a regression. A score that
          rises by more than this, or a fixed error, marks an improvement. Higher scores are better.
        </TooltipContent>
      </Tooltip>
      {invalid ? (
        <span id="regression-threshold-range" className="text-caption text-status-error">
          Between 0 and 1000
        </span>
      ) : null}
    </div>
  );
}

/** The difference of a lower-is-better value to the baseline's, coloured: a rise is worse. */
function LowerIsBetter({
  value,
  base,
  format,
}: {
  value: number | undefined;
  base: number | undefined;
  format: (value: number) => string;
}) {
  if (value === undefined) {
    return <span>—</span>;
  }
  if (base === undefined || value === base) {
    return <span className="tabular-nums">{format(value)}</span>;
  }
  const difference = value - base;
  const sign = difference > 0 ? "+" : MINUS;
  const direction = difference > 0 ? "worse" : "better";
  return (
    <span className="flex flex-wrap items-center gap-2 tabular-nums">
      <span>{format(value)}</span>
      <Badge variant="outline" className={DIRECTION_CLASS[direction]}>
        {`${sign}${format(Math.abs(difference))}`}
      </Badge>
    </span>
  );
}

/** One experiment's metadata chips and creation time, under its column header. */
function ExperimentHeader({
  summary,
  baseline,
}: {
  summary: Schema<"ExperimentSummary">;
  baseline: boolean;
}) {
  const tags = metadataTags(summary.experiment.metadata);
  return (
    <div className="flex flex-col items-start gap-1">
      <ExperimentColumnHeader experiment={summary.experiment} baseline={baseline} />
      <span className="text-caption font-normal text-muted-foreground">
        {formatRelative(summary.experiment.created_at, Date.now())}
      </span>
      {tags.length === 0 ? null : (
        <div className="flex flex-wrap gap-1">
          {tags.map(([key, text]) => (
            <Badge key={key} variant="outline" className="text-caption font-normal">
              {`${key} ${text}`}
            </Badge>
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * The summary: one row per score, one column per experiment. A candidate's cell shows its delta
 * to the baseline and its improved and regressed counts. Errors, latency and cost follow, each
 * with the candidate's difference (lower is better). Above the table, each candidate's improved
 * and regressed item counts; a click on one sets `only`.
 */
function CompareSummary({
  experiments,
  deltas,
  onOnly,
}: {
  experiments: Schema<"ExperimentSummary">[];
  deltas: Schema<"ScoreDeltas">[];
  onOnly: (only: CompareOnly) => void;
}) {
  if (experiments.length === 0) {
    return null;
  }
  const baseline = experiments[0];
  const names = scoreNames(experiments);
  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap gap-x-6 gap-y-1 text-body">
        {experiments.slice(1).map((summary) => {
          const entry = deltas.find(
            (candidate) => candidate.experiment_id === summary.experiment.id,
          );
          if (entry === undefined) {
            return null;
          }
          return (
            <span key={summary.experiment.id} className="flex flex-wrap items-center gap-1">
              <span className="font-medium">{summary.experiment.name}:</span>
              <Button
                type="button"
                variant="ghost"
                size="xs"
                className="text-status-ok"
                onClick={() => {
                  onOnly("improved");
                }}
              >
                {`${entry.improved_items} improved`}
              </Button>
              <span className="text-muted-foreground">·</span>
              <Button
                type="button"
                variant="ghost"
                size="xs"
                className="text-status-error"
                onClick={() => {
                  onOnly("regressed");
                }}
              >
                {`${entry.regressed_items} regressed`}
              </Button>
              <span className="text-muted-foreground">items</span>
            </span>
          );
        })}
      </div>
      <Table aria-label="Scores">
        <TableHeader>
          <TableRow>
            <TableHead>Score</TableHead>
            {experiments.map((summary, index) => (
              <TableHead key={summary.experiment.id}>
                <ExperimentHeader summary={summary} baseline={index === 0} />
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
                  const entry =
                    index === 0
                      ? undefined
                      : deltas.find(
                          (candidate) => candidate.experiment_id === summary.experiment.id,
                        );
                  const delta = entry?.scores[name];
                  const avg = headlineValue(score);
                  return (
                    <TableCell key={summary.experiment.id}>
                      <span className="flex flex-col items-start gap-1">
                        <span className="flex flex-wrap items-center gap-2">
                          {avg === undefined ? (
                            <span className="tabular-nums">{headlineText(score)}</span>
                          ) : (
                            <ScoreAverageBadge
                              name={name}
                              avg={avg}
                              dataType={score.data_type}
                              text={headlineText(score)}
                            />
                          )}
                          {delta === undefined || dataType === undefined ? null : (
                            <>
                              <Badge
                                variant="outline"
                                className={DIRECTION_CLASS[deltaDirection(delta.delta, dataType)]}
                              >
                                {deltaText(delta.delta, dataType)}
                                <span className="sr-only"> versus baseline</span>
                              </Badge>
                              <span className="text-caption text-status-ok">
                                {`▲ ${delta.improved} improved`}
                              </span>
                              <span className="text-caption text-status-error">
                                {`▼ ${delta.regressed} regressed`}
                              </span>
                              <span className="text-caption text-muted-foreground">
                                {`n = ${delta.n_compared}`}
                              </span>
                            </>
                          )}
                        </span>
                        <span className="text-caption text-muted-foreground">
                          {scoreSummaryText(score)}
                        </span>
                      </span>
                    </TableCell>
                  );
                })}
              </TableRow>
            );
          })}
          <TableRow>
            <TableCell>Errors</TableCell>
            {experiments.map((summary, index) => (
              <TableCell key={summary.experiment.id}>
                <LowerIsBetter
                  value={summary.error_count}
                  base={index === 0 ? undefined : baseline?.error_count}
                  format={String}
                />
              </TableCell>
            ))}
          </TableRow>
          <TableRow>
            <TableCell>Latency p50</TableCell>
            {experiments.map((summary, index) => (
              <TableCell key={summary.experiment.id}>
                <LowerIsBetter
                  value={summary.latency_p50_ns}
                  base={index === 0 ? undefined : baseline?.latency_p50_ns}
                  format={formatDuration}
                />
              </TableCell>
            ))}
          </TableRow>
          <TableRow>
            <TableCell>Cost</TableCell>
            {experiments.map((summary, index) => (
              <TableCell key={summary.experiment.id}>
                <LowerIsBetter
                  value={summary.cost_total}
                  base={index === 0 ? undefined : baseline?.cost_total}
                  format={(value) => formatCost(value)}
                />
              </TableCell>
            ))}
          </TableRow>
        </TableBody>
      </Table>
    </div>
  );
}
