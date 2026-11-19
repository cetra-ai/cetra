import { Link } from "@tanstack/react-router";
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
import { itemKey, itemShortId, scoreValueText } from "@/lib/experiments";
import { formatCost, formatDuration } from "@/lib/format";

/** Cells that are a link or control: a click there does not open the item. */
const INTERACTIVE = "a, button, input, [role='checkbox'], [role='menu'], [role='menuitem']";

export function ResultsTable({
  rows,
  scoreNames,
  repetitions,
  activeKey,
  onOpen,
}: {
  rows: Schema<"ResultRow">[];
  /** The summary's score names, in order. */
  scoreNames: string[];
  repetitions: number;
  /** The item key (`itemKey`) of the open item view, if any. */
  activeKey?: string;
  /** A click on a row (not on its links or controls) opens the item view. */
  onOpen: (row: Schema<"ResultRow">) => void;
}) {
  const { org, project } = useProject();
  return (
    <Table aria-label="Results">
      <TableHeader>
        <TableRow>
          <TableHead>Item</TableHead>
          {repetitions > 1 ? <TableHead>Rep</TableHead> : null}
          <TableHead>Input</TableHead>
          <TableHead>Expected</TableHead>
          <TableHead>Output</TableHead>
          <TableHead>Error</TableHead>
          {scoreNames.map((name) => (
            <TableHead key={name}>{name}</TableHead>
          ))}
          <TableHead>Latency</TableHead>
          <TableHead>Cost</TableHead>
          <TableHead>Trace</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((row) => (
          <TableRow
            key={itemKey(row)}
            data-item-key={itemKey(row)}
            aria-selected={activeKey === itemKey(row)}
            className="cursor-pointer"
            onClick={(event) => {
              if (event.target instanceof Element && event.target.closest(INTERACTIVE) !== null) {
                return;
              }
              onOpen(row);
            }}
          >
            <TableCell>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="font-mono text-body">{itemShortId(row.dataset_item_id)}</span>
                </TooltipTrigger>
                <TooltipContent>{row.dataset_item_id}</TooltipContent>
              </Tooltip>
            </TableCell>
            {repetitions > 1 ? (
              <TableCell className="tabular-nums">{row.repetition + 1}</TableCell>
            ) : null}
            <TableCell>
              <JsonCell value={row.input} />
            </TableCell>
            <TableCell>
              <JsonCell value={row.expected_output} />
            </TableCell>
            <TableCell>
              <JsonCell value={row.output} />
            </TableCell>
            <TableCell>
              {row.error === undefined ? null : (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Badge variant="destructive">Error</Badge>
                  </TooltipTrigger>
                  <TooltipContent>{row.error}</TooltipContent>
                </Tooltip>
              )}
            </TableCell>
            {scoreNames.map((name) => {
              const value = row.scores[name];
              const failed = value?.data_type === "boolean" && value.value_num === 0;
              return (
                <TableCell
                  key={name}
                  className={failed ? "tabular-nums text-status-error" : "tabular-nums"}
                >
                  {value === undefined ? "—" : scoreValueText(value)}
                </TableCell>
              );
            })}
            <TableCell className="tabular-nums">
              {row.latency_ns === undefined ? "—" : formatDuration(row.latency_ns)}
            </TableCell>
            <TableCell className="tabular-nums">{formatCost(row.cost_total)}</TableCell>
            <TableCell>
              {row.trace_id === undefined ? null : (
                <Link
                  to="/$org/$project/traces/$traceId"
                  params={{ org, project, traceId: row.trace_id }}
                  className="rounded-sm text-body underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                >
                  trace
                </Link>
              )}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

/** A cell's truncated preview; the item view shows the whole value. */
function JsonCell({ value }: { value: unknown }) {
  return <span className="block max-w-xs truncate font-mono text-body">{previewJson(value)}</span>;
}
