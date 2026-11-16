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
import { scoreValueText } from "@/lib/experiments";
import { formatCost, formatDuration, shortId } from "@/lib/format";

/** The full JSON of a cell is shown in a tooltip up to this many characters. */
const TOOLTIP_CHARS = 2000;

export function ResultsTable({
  rows,
  scoreNames,
  repetitions,
}: {
  rows: Schema<"ResultRow">[];
  /** The summary's score names, in order. */
  scoreNames: string[];
  repetitions: number;
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
          <TableRow key={`${row.dataset_item_id}-${row.repetition}`}>
            <TableCell>
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="font-mono text-xs">{shortId(row.dataset_item_id)}</span>
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
                  className="rounded-sm text-xs underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
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

function JsonCell({ value }: { value: unknown }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="block max-w-xs truncate font-mono text-xs">{previewJson(value)}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-lg break-all font-mono text-xs">
        <span className="block whitespace-pre-wrap">{previewJson(value, TOOLTIP_CHARS)}</span>
      </TooltipContent>
    </Tooltip>
  );
}
