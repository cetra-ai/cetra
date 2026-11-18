import { formatValue } from "@/lib/dashboards/format";
import type { TablePlot } from "@/lib/dashboards/plot";

/**
 * A table of the groups of a total answer, one column per measure. With `share`, the first
 * measure's share of the total is a bar under its value. A model row opens that model's traces.
 */
export function WidgetTable({
  plot,
  breakdownField,
  onModel,
}: {
  plot: TablePlot;
  /** The split's field: a `model` or `models` group gets the "open traces" button. */
  breakdownField?: string;
  onModel?: (model: string) => void;
}) {
  const modelGroups = breakdownField === "model" || breakdownField === "models";
  return (
    <div className="min-h-0 flex-1 overflow-auto">
      <table className="w-full text-left text-caption">
        <thead className="text-muted-foreground">
          <tr>
            <th className="w-2/5 py-1 font-medium">{breakdownField ?? "Group"}</th>
            {plot.columns.map((column, index) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: measures are positional; labels can repeat
              <th key={index} className="py-1 text-right font-medium">
                {column.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {plot.rows.map((row, index) => {
            const label = row.other ? "Other" : (row.group ?? "Total");
            return (
              <tr key={row.group ?? `total-${index}`} className="border-t border-border/60">
                <td className="max-w-0 truncate py-1 pr-2 font-mono" title={label}>
                  {modelGroups && onModel && row.group && !row.other ? (
                    <button
                      type="button"
                      aria-label={`Open traces of ${row.group}`}
                      className="focus-ring max-w-full truncate rounded-sm text-left font-mono"
                      onClick={() => onModel(row.group as string)}
                    >
                      {row.group}
                    </button>
                  ) : (
                    label
                  )}
                </td>
                {plot.columns.map((column, position) => (
                  // biome-ignore lint/suspicious/noArrayIndexKey: measures are positional; labels can repeat
                  <td key={position} className="py-1 text-right font-mono tabular-nums">
                    {formatValue(column.kind, row.values[position] ?? null)}
                    {position === 0 && row.share !== undefined ? (
                      <div
                        aria-hidden="true"
                        className="ml-auto mt-0.5 h-0.5 rounded-full bg-chart-1"
                        style={{ width: `${row.share * 100}%` }}
                      />
                    ) : null}
                  </td>
                ))}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
