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
import { DIRECTION_CLASS, deltaDirection, deltaText, itemShortId } from "@/lib/experiments";

type DataType = Schema<"ScoreSummary">["data_type"];

/** The output preview of a cell: at most this many characters, shown in three lines. */
const OUTPUT_PREVIEW_CHARS = 300;

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

/** Score names in the baseline's order, then the names only the other experiments have. */
export function scoreNames(experiments: readonly Schema<"ExperimentSummary">[]): string[] {
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

/** The data type of each score name, from the first experiment that has it. */
export function scoreDataTypes(
  experiments: readonly Schema<"ExperimentSummary">[],
): ReadonlyMap<string, DataType> {
  const types = new Map<string, DataType>();
  for (const experiment of experiments) {
    for (const score of experiment.scores) {
      if (!types.has(score.name)) {
        types.set(score.name, score.data_type);
      }
    }
  }
  return types;
}

/** A click on a link or a button inside a row belongs to that control, not to the row. */
function isControl(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("a, button, [role='button']") !== null;
}

/**
 * The items of a comparison, one row each. The experiments come baseline first. A cell is
 * matched to its column by `experiment_id`, never by position (D-J-13); a column without a cell
 * shows "Not run". A row click calls `onOpenItem` with the item id.
 */
export function CompareTable({
  rows,
  experiments,
  onOpenItem,
}: {
  rows: Schema<"CompareRow">[];
  experiments: Schema<"ExperimentSummary">[];
  onOpenItem: (datasetItemId: string) => void;
}) {
  const baselineId = experiments[0]?.experiment.id;
  const dataTypes = scoreDataTypes(experiments);
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
          const baselineCell = row.cells.find((cell) => cell.experiment_id === baselineId);
          return (
            <TableRow
              key={row.dataset_item_id}
              className="cursor-pointer"
              onClick={(event) => {
                if (!isControl(event.target)) {
                  onOpenItem(row.dataset_item_id);
                }
              }}
            >
              <TableCell>
                <span className="font-mono text-body">{itemShortId(row.dataset_item_id)}</span>
              </TableCell>
              <TableCell>
                <span className="block max-w-[200px] truncate font-mono text-body">
                  {previewJson(row.input)}
                </span>
              </TableCell>
              <TableCell>
                <span className="block max-w-[200px] truncate font-mono text-body">
                  {previewJson(row.expected_output)}
                </span>
              </TableCell>
              {experiments.map((summary, index) => {
                const cell = row.cells.find(
                  (candidate) => candidate.experiment_id === summary.experiment.id,
                );
                return (
                  <TableCell key={summary.experiment.id} className={cellTint(cell)}>
                    {cell === undefined ? (
                      <span className="text-body text-muted-foreground">Not run</span>
                    ) : (
                      <CellView
                        cell={cell}
                        baseline={index === 0}
                        baselineScores={index === 0 ? undefined : baselineCell?.scores}
                        dataTypes={dataTypes}
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

/** The cell's background: red when regressed, green when improved (regressed wins). */
function cellTint(cell: Schema<"CompareCell"> | undefined): string {
  if (cell?.regressed) {
    return "bg-status-error/10 align-top";
  }
  if (cell?.improved) {
    return "bg-status-ok/10 align-top";
  }
  return "align-top";
}

/**
 * One cell: the output preview, the error, and the scores. A candidate's scores show the
 * difference to the baseline's value for the item, coloured by direction; the baseline's show
 * their values. A ▲ or ▼ in the corner marks an improved or regressed cell.
 */
const CellView = memo(function CellView({
  cell,
  baseline,
  baselineScores,
  dataTypes,
}: {
  cell: Schema<"CompareCell">;
  baseline: boolean;
  baselineScores: Record<string, number> | undefined;
  dataTypes: ReadonlyMap<string, DataType>;
}) {
  const scores = Object.entries(cell.scores);
  return (
    <div className="relative flex max-w-sm flex-col gap-2">
      {cell.improved || cell.regressed ? (
        <div className="absolute top-0 right-0 flex gap-1">
          {cell.improved ? (
            <span title="Improved" className="text-body leading-none text-status-ok">
              ▲<span className="sr-only">Improved</span>
            </span>
          ) : null}
          {cell.regressed ? (
            <span title="Regressed" className="text-body leading-none text-status-error">
              ▼<span className="sr-only">Regressed</span>
            </span>
          ) : null}
        </div>
      ) : null}
      {cell.output === undefined ? (
        <p className="text-body text-muted-foreground">No output</p>
      ) : (
        <p className="line-clamp-3 font-mono text-body whitespace-pre-wrap">
          {previewJson(cell.output, OUTPUT_PREVIEW_CHARS)}
        </p>
      )}
      {cell.error === undefined ? null : (
        <Tooltip>
          <TooltipTrigger asChild>
            <Badge variant="destructive">Error</Badge>
          </TooltipTrigger>
          <TooltipContent>{cell.error}</TooltipContent>
        </Tooltip>
      )}
      {scores.length > 0 ? (
        <div className="flex flex-wrap gap-1">
          {scores.map(([name, value]) => {
            const dataType = dataTypes.get(name);
            if (baseline) {
              return (
                <span key={name} className="rounded border px-1.5 text-caption tabular-nums">
                  {`${name} ${value.toFixed(3)}`}
                </span>
              );
            }
            const baseValue = baselineScores?.[name];
            if (baseValue === undefined || dataType === undefined) {
              return null;
            }
            const difference = value - baseValue;
            return (
              <Badge
                key={name}
                variant="outline"
                className={`text-caption tabular-nums ${DIRECTION_CLASS[deltaDirection(difference, dataType)]}`}
              >
                {`${name} ${deltaText(difference, dataType)}`}
              </Badge>
            );
          })}
        </div>
      ) : null}
    </div>
  );
});
