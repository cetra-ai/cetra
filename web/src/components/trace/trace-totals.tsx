import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";

/** Trace count, error count and cost of a set of traces (D-C-31). */
export type Totals = { traceCount: number; errorTraceCount: number; costTotal: number };

/** The API's totals as `Totals`. */
export function totalsOf(data: Schema<"TraceTotals">): Totals {
  return {
    traceCount: data.trace_count,
    errorTraceCount: data.error_trace_count,
    costTotal: data.cost_total,
  };
}

/**
 * Totals of the checked rows: the count is every checked id; errors and cost come from the loaded
 * rows among them (status `error`; a missing cost adds 0).
 */
export function selectionTotals(
  rows: readonly Schema<"TraceSummary">[],
  ids: ReadonlySet<string>,
): Totals {
  const totals: Totals = { traceCount: ids.size, errorTraceCount: 0, costTotal: 0 };
  for (const row of rows) {
    if (ids.has(row.trace_id)) {
      if (row.status === "error") {
        totals.errorTraceCount += 1;
      }
      totals.costTotal += row.cost_total ?? 0;
    }
  }
  return totals;
}

const number = (value: number) => value.toLocaleString("en-US");

/** `1 trace`, `2 traces`, `1,204 traces`: en-US grouping, singular for exactly 1. */
export function countText(n: number, singular: string, plural: string): string {
  return `${number(n)} ${n === 1 ? singular : plural}`;
}

type Props = {
  /** Totals of the checked rows; undefined when no row is checked. */
  selected: Totals | undefined;
  /** Totals of the whole list once loaded. */
  list: Totals | undefined;
  /** The list totals query's status. */
  listStatus: "pending" | "error" | "success";
};

function Sep() {
  return <span aria-hidden="true">{" · "}</span>;
}

function Parts({ first, totals }: { first: string; totals: Totals }) {
  return (
    <>
      <span>{first}</span>
      <Sep />
      <span className={totals.errorTraceCount > 0 ? "text-destructive" : undefined}>
        {countText(totals.errorTraceCount, "error", "errors")}
      </span>
      <Sep />
      <span className="num" title="Total cost">
        {formatCost(totals.costTotal)}
      </span>
    </>
  );
}

export function TraceTotalsLine({ selected, list, listStatus }: Props) {
  return (
    <section
      aria-label="Totals"
      className="flex h-7 shrink-0 items-center border-t border-border px-3 text-caption text-muted-foreground"
    >
      {selected !== undefined ? (
        <Parts
          first={
            list
              ? `${number(selected.traceCount)} of ${number(list.traceCount)} selected`
              : `${number(selected.traceCount)} selected`
          }
          totals={selected}
        />
      ) : listStatus === "success" && list ? (
        <Parts first={countText(list.traceCount, "trace", "traces")} totals={list} />
      ) : listStatus === "error" ? (
        "Totals unavailable"
      ) : (
        "Counting…"
      )}
    </section>
  );
}
