import { Link } from "@tanstack/react-router";
import { useState } from "react";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { DIRECTION_CLASS, deltaDirection, deltaText, itemShortId } from "@/lib/experiments";
import { formatDuration } from "@/lib/format";
import { ExperimentColumnHeader, scoreDataTypes } from "./compare-table";
import { useArrowStep, ValueView } from "./item-view";
import { OutputDiff } from "./output-diff";

type Cell = Schema<"CompareCell">;

/**
 * One item of a comparison in a right-hand sheet: its input and expected output, then one column
 * per experiment (baseline first) with its output, scores, latency, error and trace. Up and down
 * step through `onStep`. `row` undefined closes the sheet.
 */
export function CompareItemSheet({
  row,
  experiments,
  onStep,
  onClose,
}: {
  row: Schema<"CompareRow"> | undefined;
  experiments: Schema<"ExperimentSummary">[];
  onStep: (delta: 1 | -1) => void;
  onClose: () => void;
}) {
  const open = row !== undefined;
  useArrowStep(open, onStep);
  return (
    <Sheet
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          onClose();
        }
      }}
    >
      <SheetContent className="w-full overflow-y-auto sm:max-w-4xl">
        {row === undefined ? null : (
          <>
            <SheetHeader>
              <SheetTitle className="text-title">{`Item ${itemShortId(row.dataset_item_id)}`}</SheetTitle>
              <SheetDescription className="text-body">
                Up and down step to the other items.
              </SheetDescription>
            </SheetHeader>
            <CompareItem row={row} experiments={experiments} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}

function CompareItem({
  row,
  experiments,
}: {
  row: Schema<"CompareRow">;
  experiments: Schema<"ExperimentSummary">[];
}) {
  const baselineId = experiments[0]?.experiment.id;
  const baselineCell = row.cells.find((cell) => cell.experiment_id === baselineId);
  const dataTypes = scoreDataTypes(experiments);
  return (
    <div className="flex flex-col gap-5 p-4 text-ui">
      <div className="flex items-center gap-1 font-mono text-caption text-muted-foreground">
        <span className="break-all">{row.dataset_item_id}</span>
      </div>
      <ValueView label="Input" value={row.input} />
      <ValueView label="Expected output" value={row.expected_output} />
      <div className="flex gap-4 overflow-x-auto pb-1">
        {experiments.map((summary, index) => {
          const cell = row.cells.find(
            (candidate) => candidate.experiment_id === summary.experiment.id,
          );
          return (
            <section
              key={summary.experiment.id}
              aria-label={summary.experiment.name}
              className="flex min-w-[20rem] flex-1 flex-col gap-4"
            >
              <ExperimentColumnHeader experiment={summary.experiment} baseline={index === 0} />
              {cell === undefined ? (
                <span className="text-body text-muted-foreground">Not run</span>
              ) : (
                <CellColumn
                  cell={cell}
                  baseline={index === 0}
                  baselineCell={baselineCell}
                  dataTypes={dataTypes}
                  experimentId={summary.experiment.id}
                />
              )}
            </section>
          );
        })}
      </div>
    </div>
  );
}

function CellColumn({
  cell,
  baseline,
  baselineCell,
  dataTypes,
  experimentId,
}: {
  cell: Cell;
  baseline: boolean;
  baselineCell: Cell | undefined;
  dataTypes: ReadonlyMap<string, Schema<"ScoreSummary">["data_type"]>;
  experimentId: string;
}) {
  const { org, project } = useProject();
  const [diff, setDiff] = useState(false);
  const scores = Object.entries(cell.scores);
  return (
    <>
      {cell.error === undefined ? null : (
        <div
          role="alert"
          className="rounded-md border border-status-error px-3 py-2 text-body break-words whitespace-pre-wrap text-status-error"
        >
          {cell.error}
        </div>
      )}
      <section aria-label={`Output of ${experimentId}`} className="flex flex-col gap-2">
        {baseline ? (
          <ValueView label="Output" value={cell.output} />
        ) : (
          <>
            <div className="flex items-center gap-1">
              <h3 className="text-body font-medium text-muted-foreground">Output</h3>
              <Button
                type="button"
                variant={diff ? "secondary" : "ghost"}
                size="xs"
                className="ml-auto"
                aria-pressed={diff}
                onClick={() => {
                  setDiff((current) => !current);
                }}
              >
                Diff vs baseline
              </Button>
            </div>
            {diff ? (
              <OutputDiff before={baselineCell?.output} after={cell.output} />
            ) : (
              <ValueView label="Output" value={cell.output} />
            )}
          </>
        )}
      </section>
      <section aria-label="Scores" className="flex flex-col gap-2">
        <h3 className="text-body font-medium text-muted-foreground">Scores</h3>
        {scores.length === 0 ? (
          <span className="text-body text-muted-foreground">No scores</span>
        ) : (
          <ul className="flex flex-col gap-1">
            {scores.map(([name, value]) => {
              const dataType = dataTypes.get(name);
              const baseValue = baselineCell?.scores[name];
              const showDelta = !baseline && dataType !== undefined && baseValue !== undefined;
              return (
                <li key={name} className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{name}</span>
                  <span className="tabular-nums">{value.toFixed(3)}</span>
                  {showDelta ? (
                    <Badge
                      variant="outline"
                      className={DIRECTION_CLASS[deltaDirection(value - baseValue, dataType)]}
                    >
                      {deltaText(value - baseValue, dataType)}
                    </Badge>
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
      </section>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-body text-muted-foreground">
        <span className="tabular-nums">
          {cell.latency_ns === undefined
            ? "Latency —"
            : `Latency ${formatDuration(cell.latency_ns)}`}
        </span>
        {cell.trace_id === undefined ? null : (
          <Link
            to="/$org/$project/traces/$traceId"
            params={{ org, project, traceId: cell.trace_id }}
            className="rounded-sm underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
          >
            Open trace
          </Link>
        )}
      </div>
    </>
  );
}
