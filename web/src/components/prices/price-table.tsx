import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { escapeRegex, formatUnitPrice, isAlwaysValid } from "@/lib/prices";

export function PriceTable({
  label,
  rows,
  actions,
}: {
  /** The table's accessible name. */
  label: string;
  rows: Schema<"ModelPrice">[];
  /** Per-row actions (Edit, Delete); absent for read-only tables. */
  actions?: (row: Schema<"ModelPrice">) => ReactNode;
}) {
  return (
    <Table aria-label={label}>
      <TableHeader>
        <TableRow>
          <TableHead>Model</TableHead>
          <TableHead>Provider</TableHead>
          <TableHead className="text-right">Input</TableHead>
          <TableHead className="text-right">Output</TableHead>
          <TableHead className="text-right">Cache read</TableHead>
          <TableHead className="text-right">Cache write</TableHead>
          <TableHead>Valid from</TableHead>
          <TableHead className="text-right">Tiers</TableHead>
          {actions ? (
            <TableHead>
              <span className="sr-only">Actions</span>
            </TableHead>
          ) : null}
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((row) => (
          <TableRow key={row.id ?? `builtin:${row.match_pattern}`}>
            <TableCell>
              <div className="flex flex-col gap-0.5">
                <span>{row.model_name}</span>
                {row.match_pattern !== escapeRegex(row.model_name) ? (
                  <span className="font-mono text-xs text-muted-foreground">
                    {row.match_pattern}
                  </span>
                ) : null}
              </div>
            </TableCell>
            <TableCell>{row.provider ?? "Any"}</TableCell>
            <TableCell className="text-right tabular-nums">
              {formatUnitPrice(row.price_input)}
            </TableCell>
            <TableCell className="text-right tabular-nums">
              {formatUnitPrice(row.price_output)}
            </TableCell>
            <TableCell className="text-right tabular-nums">
              {formatUnitPrice(row.price_cache_read)}
            </TableCell>
            <TableCell className="text-right tabular-nums">
              {formatUnitPrice(row.price_cache_write)}
            </TableCell>
            <TableCell>
              {isAlwaysValid(row.valid_from) ? "Always" : row.valid_from.slice(0, 10)}
            </TableCell>
            <TableCell className="text-right tabular-nums">{row.tiers.length || "—"}</TableCell>
            {actions ? <TableCell>{actions(row)}</TableCell> : null}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}
