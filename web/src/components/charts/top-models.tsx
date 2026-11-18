import type { Schema } from "@/api/types";
import { formatCost, formatTokens } from "@/lib/format";
import { formatCount } from "@/lib/metrics";

export const TOP_MODELS_TITLE = "Top models by cost";
export const TOP_MODELS_UNIT = "USD";

export function TopModels({
  models,
  onModel,
}: {
  models: readonly Schema<"ModelUsage">[];
  onModel?: (model: string) => void;
}) {
  // Each row's bar is its share of the listed models' cost.
  const listed = models.reduce((sum, model) => sum + (model.cost_total ?? 0), 0);
  const label =
    models.length === 0
      ? `${TOP_MODELS_TITLE}: no data in range`
      : `${TOP_MODELS_TITLE}: ${models
          .map((model) => `${model.model} ${formatCost(model.cost_total)}`)
          .join(", ")} in range`;
  return (
    <section aria-label={label} className="flex h-[220px] min-w-0 flex-col rounded-md border p-3">
      <CardHeading title={TOP_MODELS_TITLE} unit={TOP_MODELS_UNIT} />
      {models.length === 0 ? (
        <p className="flex flex-1 items-center justify-center text-muted-foreground text-body">
          No data
        </p>
      ) : (
        <div className="mt-2 min-h-0 flex-1 overflow-auto">
          <table className="w-full text-left text-caption">
            <thead className="text-muted-foreground">
              <tr>
                <th className="py-1 font-medium">Model</th>
                <th className="py-1 text-right font-medium">LLM calls</th>
                <th className="py-1 text-right font-medium">Tokens</th>
                <th className="py-1 text-right font-medium">Cost</th>
              </tr>
            </thead>
            <tbody>
              {models.map((model) => {
                const share = listed > 0 ? ((model.cost_total ?? 0) / listed) * 100 : 0;
                return (
                  <tr
                    key={model.model}
                    className={`border-t border-border/60 ${onModel ? "cursor-pointer hover:bg-muted/50" : ""}`}
                    // A click on the row opens the traces of its model, as a mouse shortcut to the button.
                    onClick={onModel ? () => onModel(model.model) : undefined}
                  >
                    <td className="max-w-0 truncate py-1 pr-2 font-mono">
                      {onModel ? (
                        <button
                          type="button"
                          aria-label={`Open traces of ${model.model}`}
                          className="focus-ring max-w-full truncate rounded-sm text-left font-mono"
                          onClick={(event) => {
                            // The row's own click would open the same traces a second time.
                            event.stopPropagation();
                            onModel(model.model);
                          }}
                        >
                          {model.model}
                        </button>
                      ) : (
                        model.model
                      )}
                    </td>
                    <td className="py-1 text-right font-mono tabular-nums">
                      {formatCount(model.llm_count)}
                    </td>
                    <td className="py-1 text-right font-mono tabular-nums">
                      {formatTokens(model.total_tokens)}
                    </td>
                    <td className="py-1 text-right font-mono tabular-nums">
                      {formatCost(model.cost_total)}
                      <div
                        aria-hidden="true"
                        className="ml-auto mt-0.5 h-0.5 rounded-full bg-chart-1"
                        style={{ width: `${share}%` }}
                      />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/** A card's title and unit. */
export function CardHeading({ title, unit }: { title: string; unit: string }) {
  return (
    <header className="flex items-baseline justify-between gap-2">
      <h2 className="text-body font-medium">{title}</h2>
      <span className="font-mono text-caption text-muted-foreground">{unit}</span>
    </header>
  );
}
