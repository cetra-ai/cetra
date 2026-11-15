import { Link } from "@tanstack/react-router";
import { memo } from "react";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { previewJson } from "@/lib/datasets";
import { formatDuration, shortId } from "@/lib/format";
import { OutputDiff } from "./output-diff";

/** The baseline's output is shown this many characters long. */
const BASELINE_OUTPUT_CHARS = 300;

/** A column header: the experiment's name linked to its page, with "Baseline" on the first. */
export function ExperimentColumnHeader({
  experiment,
  baseline,
}: {
  experiment: Schema<"Experiment">;
  baseline: boolean;
}) {
  const { org, project } = useProject();
  return (
    <span className="flex flex-col items-start gap-1">
      <Link
        to="/$org/$project/experiments/$experimentId"
        params={{ org, project, experimentId: experiment.id }}
        className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
      >
        {experiment.name}
      </Link>
      {baseline ? <Badge variant="secondary">Baseline</Badge> : null}
    </span>
  );
}

/**
 * The items of a comparison, one row each. The experiments come baseline first. A cell is
 * matched to its column by `experiment_id`, never by position (D-J-13); a column without a cell
 * shows "Not run".
 */
export function CompareTable({
  rows,
  experiments,
}: {
  rows: Schema<"CompareRow">[];
  experiments: Schema<"ExperimentSummary">[];
}) {
  const baselineId = experiments[0]?.experiment.id;
  return (
    <Table aria-label="Items">
      <TableHeader>
        <TableRow>
          <TableHead>Item</TableHead>
          <TableHead>Input</TableHead>
          <TableHead>Expected</TableHead>
          {experiments.map((summary, index) => (
            <TableHead key={summary.experiment.id}>
              <ExperimentColumnHeader experiment={summary.experiment} baseline={index === 0} />
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((row) => {
          const regressed = row.cells.some((cell) => cell.regressed);
          const baselineCell = row.cells.find((cell) => cell.experiment_id === baselineId);
          return (
            <TableRow key={row.dataset_item_id} data-regressed={regressed ? "true" : undefined}>
              <TableCell>
                <span className="font-mono text-xs">{shortId(row.dataset_item_id)}</span>
              </TableCell>
              <TableCell>
                <span className="block max-w-xs truncate font-mono text-xs">
                  {previewJson(row.input)}
                </span>
              </TableCell>
              <TableCell>
                <span className="block max-w-xs truncate font-mono text-xs">
                  {previewJson(row.expected_output)}
                </span>
              </TableCell>
              {experiments.map((summary, index) => {
                const cell = row.cells.find(
                  (candidate) => candidate.experiment_id === summary.experiment.id,
                );
                return (
                  <TableCell
                    key={summary.experiment.id}
                    className={cell?.regressed ? "bg-status-error/10 align-top" : "align-top"}
                  >
                    {cell === undefined ? (
                      <span className="text-muted-foreground">Not run</span>
                    ) : (
                      <CellView
                        cell={cell}
                        baseline={index === 0}
                        baselineOutput={baselineCell?.output}
                      />
                    )}
                  </TableCell>
                );
              })}
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
}

/** One cell. Memoized so that loading more rows does not re-diff the rows already shown. */
const CellView = memo(function CellView({
  cell,
  baseline,
  baselineOutput,
}: {
  cell: Schema<"CompareCell">;
  baseline: boolean;
  baselineOutput: unknown;
}) {
  const { org, project } = useProject();
  const scores = Object.entries(cell.scores);
  return (
    <div className="flex max-w-sm flex-col gap-2">
      {cell.output === undefined ? (
        <p className="text-xs text-muted-foreground">No output</p>
      ) : baseline ? (
        <p className="whitespace-pre-wrap font-mono text-xs">
          {previewJson(cell.output, BASELINE_OUTPUT_CHARS)}
        </p>
      ) : (
        <OutputDiff before={baselineOutput} after={cell.output} />
      )}
      {cell.error === undefined ? null : (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="destructive">Error</Badge>
          </TooltipTrigger>
          <TooltipContent>{cell.error}</TooltipContent>
        </Tooltip>
      )}
      {cell.regressed ? <Badge variant="destructive">Regressed</Badge> : null}
      {scores.length > 0 ? (
        <div className="flex flex-wrap gap-1">
          {scores.map(([name, value]) => (
            <span key={name} className="rounded border px-1.5 text-xs tabular-nums">
              {`${name} ${value.toFixed(3)}`}
            </span>
          ))}
        </div>
      ) : null}
      <div className="flex flex-wrap gap-x-3 text-xs text-muted-foreground">
        {cell.latency_ns === undefined ? null : <span>{formatDuration(cell.latency_ns)}</span>}
        {cell.trace_id === undefined ? null : (
          <Link
            to="/$org/$project/traces/$traceId"
            params={{ org, project, traceId: cell.trace_id }}
            className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
          >
            trace
          </Link>
        )}
      </div>
    </div>
  );
});
