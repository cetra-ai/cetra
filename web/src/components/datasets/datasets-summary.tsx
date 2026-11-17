import type { Schema } from "@/api/types";
import { RelativeTime } from "@/components/relative-time";

/**
 * The strip above the datasets list: the loaded datasets, their items and experiments, and when
 * the newest of them changed. `more` marks the totals as partial (a next page exists).
 */
export function DatasetsSummary({
  datasets,
  more,
}: {
  datasets: readonly Schema<"Dataset">[];
  more: boolean;
}) {
  const items = datasets.reduce((sum, dataset) => sum + dataset.item_count, 0);
  const experiments = datasets.reduce((sum, dataset) => sum + dataset.experiment_count, 0);
  const newest = datasets.reduce<string | undefined>((latest, dataset) => {
    if (latest === undefined) {
      return dataset.updated_at;
    }
    return Date.parse(dataset.updated_at) > Date.parse(latest) ? dataset.updated_at : latest;
  }, undefined);
  const partial = more ? "+" : "";

  return (
    <section
      aria-label="Summary"
      className="flex h-8 shrink-0 items-center gap-4 overflow-x-auto border-b px-3 text-body"
    >
      <Item label="Datasets" value={`${datasets.length}${partial}`} />
      <Item label="Items" value={`${items}${partial}`} />
      <Item label="Experiments" value={`${experiments}${partial}`} />
      <span className="shrink-0">
        <span className="text-muted-foreground">Last updated</span>{" "}
        {newest === undefined ? <span className="num">—</span> : <RelativeTime value={newest} />}
      </span>
    </section>
  );
}

function Item({ label, value }: { label: string; value: string }) {
  return (
    <span className="shrink-0">
      <span className="text-muted-foreground">{label}</span> <span className="num">{value}</span>
    </span>
  );
}
