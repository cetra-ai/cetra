import { cn } from "cn";
import type { Schema } from "@/api/types";
import type { DataTableColumn } from "@/components/data-table/data-table";
import {
  scoreTrend,
  TrendLine,
  UnavailableScore,
  useDatasetTrend,
} from "@/components/datasets/dataset-trend";
import { RelativeTime } from "@/components/relative-time";
import { Skeleton } from "@/components/ui/skeleton";

const STATUS_DOT: Record<string, string> = {
  completed: "bg-primary",
  failed: "bg-destructive",
  running: "bg-muted-foreground",
};

/**
 * The columns of the datasets list. The trend cells read each dataset's latest experiments from
 * `DatasetTrendsContext`. Rows are not sorted on the client, so no column sorts.
 */
export const DATASET_COLUMNS: DataTableColumn<Schema<"Dataset">>[] = [
  {
    id: "name",
    size: 240,
    header: "Name",
    enableSorting: false,
    cell: ({ row }) => <NameCell dataset={row.original} />,
  },
  {
    id: "description",
    size: 280,
    header: "Description",
    enableSorting: false,
    meta: { grow: true },
    cell: ({ row }) =>
      row.original.description ? (
        <span className="block truncate text-muted-foreground" title={row.original.description}>
          {row.original.description}
        </span>
      ) : null,
  },
  {
    id: "items",
    size: 80,
    header: "Items",
    enableSorting: false,
    meta: { align: "right" },
    cell: ({ row }) => <span className="num">{row.original.item_count}</span>,
  },
  {
    id: "version",
    size: 72,
    header: "Version",
    enableSorting: false,
    cell: ({ row }) => (
      <span className="num text-muted-foreground">v{row.original.current_version}</span>
    ),
  },
  {
    id: "experiments",
    size: 104,
    header: "Experiments",
    enableSorting: false,
    meta: { align: "right" },
    cell: ({ row }) => <span className="num">{row.original.experiment_count}</span>,
  },
  {
    id: "last-experiment",
    size: 232,
    header: "Last experiment",
    enableSorting: false,
    cell: ({ row }) => <LastExperimentCell datasetId={row.original.id} />,
  },
  {
    id: "score",
    size: 152,
    header: "Score",
    enableSorting: false,
    meta: { align: "right" },
    cell: ({ row }) => <ScoreCell datasetId={row.original.id} />,
  },
  {
    id: "created",
    size: 104,
    header: "Created",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.created_at} />,
  },
  {
    id: "updated",
    size: 104,
    header: "Updated",
    enableSorting: false,
    cell: ({ row }) => <RelativeTime value={row.original.updated_at} />,
  },
];

/** The latest experiment: its name, status and creation time. */
function LastExperimentCell({ datasetId }: { datasetId: string }) {
  const trend = useDatasetTrend(datasetId);
  if (!trend) {
    return <Skeleton className="h-4 w-32" />;
  }
  const latest = trend.experiments[0];
  if (!latest) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <span
        aria-hidden="true"
        className={cn("size-1.5 shrink-0 rounded-full", STATUS_DOT[latest.status])}
      />
      <span className="sr-only">{latest.status}</span>
      <span className="truncate">{latest.name}</span>
      <span className="shrink-0 text-muted-foreground">
        <RelativeTime value={latest.created_at} />
      </span>
    </span>
  );
}

/** The latest score, and a line of the same score over the last experiments. */
function ScoreCell({ datasetId }: { datasetId: string }) {
  const trend = useDatasetTrend(datasetId);
  if (!trend) {
    return <Skeleton className="h-4 w-20" />;
  }
  if (trend.unavailable) {
    return <UnavailableScore unavailable />;
  }
  const score = scoreTrend(trend.experiments);
  if (!score) {
    return <UnavailableScore unavailable={false} />;
  }
  return (
    <span className="flex items-center justify-end gap-2" title={score.name}>
      <span className="num">{score.latest}</span>
      <TrendLine
        points={score.points}
        label={`${score.name} over the last experiments`}
        className="h-4 w-20"
      />
    </span>
  );
}

/** The name, and a muted "schema" mark when the dataset has an input or expected-output schema. */
function NameCell({ dataset }: { dataset: Schema<"Dataset"> }) {
  const schema = dataset.input_schema !== undefined || dataset.expected_schema !== undefined;
  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <span className="truncate font-medium">{dataset.name}</span>
      {schema ? (
        <span
          className="shrink-0 rounded border px-1 text-caption text-muted-foreground"
          title="Has a schema"
        >
          schema
        </span>
      ) : null}
    </span>
  );
}
