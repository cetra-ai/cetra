import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import type { KeyboardEvent, ReactNode } from "react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { experimentsKey } from "@/api/queries-experiments";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import {
  type SummaryState,
  useExperimentSummaries,
} from "@/components/experiments/use-experiment-summaries";
import { ScoreAverageBadge } from "@/components/score-badge";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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
  compareBlockReason,
  DIRECTION_CLASS,
  deltaDirection,
  deltaText,
  type ExperimentStatus,
  expectedRuns,
  headlineText,
  headlineValue,
  metadataTags,
  previousRuns,
} from "@/lib/experiments";
import { formatCost, formatDuration, formatRelative } from "@/lib/format";
import { canManageDatasets } from "@/lib/roles";

/** Experiments that can be picked for one comparison (D-J-13). */
const MAX_SELECTED = 4;
/** A running experiment older than this shows its age (CG-J5). */
const STALE_RUNNING_MS = 24 * 60 * 60 * 1000;
/** Score badges per row before the "+N" overflow. */
const MAX_SCORE_BADGES = 4;
/** Metadata chips per row before the "+N" overflow. */
const MAX_METADATA_TAGS = 3;
/** Cells that are a link or control: a click there does not open the row. */
const INTERACTIVE = "a, button, input, [role='checkbox'], [role='menu'], [role='menuitem']";

export function ExperimentsTable(props: {
  /** Every loaded experiment, newest first. `status` picks the ones shown. */
  rows: Schema<"Experiment">[];
  /** Shows only the experiments with this status. Every row still counts for the deltas. */
  status?: ExperimentStatus;
  /** Selected ids in the order they were picked; the first is the baseline. */
  selected: readonly string[];
  onSelectedChange: (ids: string[]) => void;
  /** Hide the Dataset column (the dataset page's tab). */
  hideDataset?: boolean;
  /**
   * A click on a row (not on its links, controls or checkbox) opens the experiment. Enter opens
   * the focused row; with a panel open (`openId`), the arrows step to the neighbouring row.
   * `replace` is set for a step, so the arrows do not fill the history.
   */
  onOpen?: (id: string, replace?: boolean) => void;
  /** The experiment the panel shows, if any. */
  openId?: string;
}) {
  const { rows, status, selected, onSelectedChange, hideDataset, onOpen, openId } = props;
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const canManage = canManageDatasets(data.role);
  const [deleting, setDeleting] = useState<Schema<"Experiment"> | undefined>();
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/experiments/{id}");
  const summaryOf = useExperimentSummaries(rows);
  const previous = previousRuns(rows);
  const shown = status === undefined ? rows : rows.filter((row) => row.status === status);
  const now = Date.now();

  /** Enter opens the row; with a panel open, the arrows step to the neighbouring row. */
  function onRowKeyDown(event: KeyboardEvent<HTMLTableRowElement>, id: string) {
    if (onOpen === undefined || event.target !== event.currentTarget) {
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      onOpen(id);
      return;
    }
    if (openId === undefined || (event.key !== "ArrowDown" && event.key !== "ArrowUp")) {
      return;
    }
    const sibling =
      event.key === "ArrowDown"
        ? event.currentTarget.nextElementSibling
        : event.currentTarget.previousElementSibling;
    if (!(sibling instanceof HTMLElement) || sibling.dataset.experimentId === undefined) {
      return;
    }
    event.preventDefault();
    sibling.focus();
    onOpen(sibling.dataset.experimentId, true);
  }

  function closeDelete() {
    setDeleting(undefined);
    remove.reset();
  }

  function onDelete(experiment: Schema<"Experiment">) {
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: experiment.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: experimentsKey(org, project) });
          onSelectedChange(selected.filter((id) => id !== experiment.id));
          closeDelete();
        },
      },
    );
  }

  return (
    <>
      <Table aria-label="Experiments">
        <TableHeader>
          <TableRow>
            <TableHead>
              <span className="sr-only">Select</span>
            </TableHead>
            <TableHead>Name</TableHead>
            {hideDataset ? null : <TableHead>Dataset</TableHead>}
            <TableHead>Status</TableHead>
            <TableHead>Scores</TableHead>
            <TableHead>Errors</TableHead>
            <TableHead>Latency</TableHead>
            <TableHead>Cost</TableHead>
            <TableHead>Results</TableHead>
            <TableHead>Created</TableHead>
            <TableHead>
              <span className="sr-only">Actions</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {shown.map((row) => {
            const isSelected = selected.includes(row.id);
            const summary = summaryOf(row.id);
            const previousId = previous.get(row.id);
            const previousSummary = previousId === undefined ? undefined : summaryOf(previousId);
            const previousExperiment =
              previousId === undefined
                ? undefined
                : rows.find((candidate) => candidate.id === previousId);
            return (
              <TableRow
                key={row.id}
                data-experiment-id={row.id}
                tabIndex={onOpen ? 0 : undefined}
                aria-selected={openId === row.id}
                className={onOpen ? "cursor-pointer" : undefined}
                onKeyDown={
                  onOpen
                    ? (event) => {
                        onRowKeyDown(event, row.id);
                      }
                    : undefined
                }
                onClick={
                  onOpen
                    ? (event) => {
                        if (
                          event.target instanceof Element &&
                          event.target.closest(INTERACTIVE) !== null
                        ) {
                          return;
                        }
                        onOpen(row.id);
                      }
                    : undefined
                }
              >
                <TableCell>
                  <Checkbox
                    aria-label={`Select ${row.name}`}
                    checked={isSelected}
                    disabled={!isSelected && selected.length >= MAX_SELECTED}
                    onCheckedChange={(checked) => {
                      if (checked === true) {
                        if (!isSelected) {
                          onSelectedChange([...selected, row.id]);
                        }
                      } else {
                        onSelectedChange(selected.filter((id) => id !== row.id));
                      }
                    }}
                  />
                </TableCell>
                <TableCell>
                  <div className="flex flex-col gap-1">
                    <Link
                      to="/$org/$project/experiments/$experimentId"
                      params={{ org, project, experimentId: row.id }}
                      className="w-fit rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                    >
                      {row.name}
                    </Link>
                    <MetadataTags metadata={row.metadata} />
                  </div>
                </TableCell>
                {hideDataset ? null : (
                  <TableCell>
                    {row.dataset_id !== undefined ? (
                      <Link
                        to="/$org/$project/datasets/$datasetId"
                        params={{ org, project, datasetId: row.dataset_id }}
                        className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                      >
                        {`${row.dataset_name} v${row.dataset_version}`}
                      </Link>
                    ) : (
                      <span>
                        {`${row.dataset_name} v${row.dataset_version} `}
                        <span className="text-muted-foreground">(deleted)</span>
                      </span>
                    )}
                  </TableCell>
                )}
                <TableCell>
                  <StatusBadge experiment={row} />
                </TableCell>
                <TableCell>
                  <ScoresCell
                    state={summary}
                    before={
                      previousSummary?.state === "ready" ? previousSummary.summary : undefined
                    }
                    beforeName={previousExperiment?.name}
                  />
                </TableCell>
                <TableCell>
                  <SummaryCell
                    state={summary}
                    render={(value) => (
                      <span
                        className={
                          value.error_count > 0
                            ? "tabular-nums text-status-error"
                            : "tabular-nums text-muted-foreground"
                        }
                      >
                        {value.error_count}
                      </span>
                    )}
                  />
                </TableCell>
                <TableCell>
                  <SummaryCell
                    state={summary}
                    render={(value) => (
                      <span className="tabular-nums">
                        {value.latency_p50_ns === undefined
                          ? "—"
                          : `${formatDuration(value.latency_p50_ns)} · p95 ${
                              value.latency_p95_ns === undefined
                                ? "—"
                                : formatDuration(value.latency_p95_ns)
                            }`}
                      </span>
                    )}
                  />
                </TableCell>
                <TableCell>
                  <SummaryCell
                    state={summary}
                    render={(value) => (
                      <span className="tabular-nums">{formatCost(value.cost_total)}</span>
                    )}
                  />
                </TableCell>
                <TableCell className="tabular-nums">{`${row.result_count} / ${expectedRuns(row)}`}</TableCell>
                <TableCell>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <span>{formatRelative(row.created_at, now)}</span>
                    </TooltipTrigger>
                    <TooltipContent>
                      <TimeText value={row.created_at} style="datetime" />
                    </TooltipContent>
                  </Tooltip>
                </TableCell>
                <TableCell>
                  <DisabledReason
                    reason={canManage ? undefined : "Only members can delete experiments"}
                  >
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Actions of ${row.name}`}
                          disabled={!canManage}
                        >
                          <MoreHorizontal />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        <DropdownMenuItem
                          variant="destructive"
                          onSelect={() => {
                            setDeleting(row);
                          }}
                        >
                          Delete
                        </DropdownMenuItem>
                      </DropdownMenuContent>
                    </DropdownMenu>
                  </DisabledReason>
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
      <ConfirmDialog
        open={deleting !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            closeDelete();
          }
        }}
        title="Delete experiment"
        description="Deletes the experiment and its results. Its traces and scores stay in the project."
        confirmLabel="Delete"
        destructive
        pending={remove.isPending}
        error={
          remove.error
            ? problemMessage(remove.error, "The experiment could not be deleted")
            : undefined
        }
        onConfirm={() => {
          if (deleting) {
            onDelete(deleting);
          }
        }}
      />
    </>
  );
}

/** The first metadata values of an experiment as chips, the rest behind "+N". */
function MetadataTags({ metadata }: { metadata: Record<string, unknown> }) {
  const tags = metadataTags(metadata);
  if (tags.length === 0) {
    return null;
  }
  const rest = tags.slice(MAX_METADATA_TAGS);
  return (
    <div className="flex flex-wrap items-center gap-1">
      {tags.slice(0, MAX_METADATA_TAGS).map(([key, text]) => (
        <Badge
          key={key}
          variant="outline"
          className="px-1.5 py-0 text-caption font-normal text-muted-foreground"
        >
          {`${key} ${text}`}
        </Badge>
      ))}
      {rest.length > 0 ? (
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="text-caption text-muted-foreground">{`+${rest.length}`}</span>
          </TooltipTrigger>
          <TooltipContent>
            {rest.map(([key, text]) => (
              <div key={key}>{`${key} ${text}`}</div>
            ))}
          </TooltipContent>
        </Tooltip>
      ) : null}
    </div>
  );
}

/** The score badges of a run, each with its change against the previous run (`before`). */
function ScoresCell({
  state,
  before,
  beforeName,
}: {
  state: SummaryState;
  before?: Schema<"ExperimentSummary">;
  beforeName?: string;
}) {
  switch (state.state) {
    case "loading":
      return <Skeleton className="h-3 w-16" />;
    case "skipped":
      return <span className="text-body text-muted-foreground">Too large</span>;
    case "missing":
      return <span className="text-muted-foreground">—</span>;
    case "ready": {
      const { scores } = state.summary;
      const extra = scores.length - MAX_SCORE_BADGES;
      return (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-body">
          {scores.slice(0, MAX_SCORE_BADGES).map((score) => (
            <ScoreChip
              key={score.name}
              score={score}
              before={before?.scores.find((candidate) => candidate.name === score.name)}
              beforeName={beforeName}
            />
          ))}
          {extra > 0 ? <span className="text-muted-foreground">{`+${extra}`}</span> : null}
        </div>
      );
    }
  }
}

/** One score: its headline badge, then the change against the same score of the previous run. */
function ScoreChip({
  score,
  before,
  beforeName,
}: {
  score: Schema<"ScoreSummary">;
  before?: Schema<"ScoreSummary">;
  beforeName?: string;
}) {
  const value = headlineValue(score);
  const text = `${score.name} ${headlineText(score)}`;
  const badge =
    value === undefined ? (
      <span className="rounded-sm px-1 text-muted-foreground">{text}</span>
    ) : (
      <ScoreAverageBadge
        name={score.name}
        avg={value}
        dataType={score.data_type}
        text={text}
        className="rounded-sm px-1 tabular-nums"
      />
    );
  const beforeValue = before === undefined ? undefined : headlineValue(before);
  if (value === undefined || beforeValue === undefined) {
    return badge;
  }
  const delta = value - beforeValue;
  return (
    <span className="inline-flex items-center gap-1">
      {badge}
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={`tabular-nums ${DIRECTION_CLASS[deltaDirection(delta, score.data_type)]}`}
          >
            {deltaText(delta, score.data_type)}
          </span>
        </TooltipTrigger>
        <TooltipContent>{`vs ${beforeName ?? "the previous run"}`}</TooltipContent>
      </Tooltip>
    </span>
  );
}

/** A summary-based cell: a skeleton while it loads, "—" when it has no summary. */
function SummaryCell({
  state,
  render,
}: {
  state: SummaryState;
  render: (summary: Schema<"ExperimentSummary">) => ReactNode;
}) {
  switch (state.state) {
    case "loading":
      return <Skeleton className="h-3 w-16" />;
    case "ready":
      return <>{render(state.summary)}</>;
    case "skipped":
    case "missing":
      return <span className="text-muted-foreground">—</span>;
  }
}

/**
 * The selection above the table: the baseline (the first picked) and the Compare button, which
 * stays disabled with its reason until the picks can be compared (D-J-13).
 */
export function CompareSelection({ experiments }: { experiments: Schema<"Experiment">[] }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const [baseline] = experiments;
  if (baseline === undefined) {
    return null;
  }
  const reason = compareBlockReason(experiments);
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border px-3 py-2 text-ui">
      <p>{`Baseline: ${baseline.name}`}</p>
      <DisabledReason reason={reason}>
        <Button
          type="button"
          disabled={reason !== undefined}
          onClick={() => {
            // The ids in pick order, the first is the baseline (D-J-17).
            void navigate({
              to: "/$org/$project/experiments/compare",
              params: { org, project },
              search: { ids: experiments.map((experiment) => experiment.id).join(",") },
            });
          }}
        >
          Compare
        </Button>
      </DisabledReason>
    </div>
  );
}

export function StatusBadge({ experiment }: { experiment: Schema<"Experiment"> }) {
  switch (experiment.status) {
    case "completed":
      return <Badge variant="secondary">Completed</Badge>;
    case "failed": {
      const badge = <Badge variant="destructive">Failed</Badge>;
      if (experiment.error === undefined) {
        return badge;
      }
      return (
        <Tooltip>
          <TooltipTrigger asChild>{badge}</TooltipTrigger>
          <TooltipContent>{experiment.error}</TooltipContent>
        </Tooltip>
      );
    }
    case "running": {
      const started = Date.parse(experiment.created_at);
      const stale = !Number.isNaN(started) && Date.now() - started > STALE_RUNNING_MS;
      return (
        <Badge variant="outline">
          {stale
            ? `Running · started ${formatRelative(experiment.created_at, Date.now())}`
            : "Running"}
        </Badge>
      );
    }
  }
}
