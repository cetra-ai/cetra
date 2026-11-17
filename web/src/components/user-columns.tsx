import { cn } from "cn";
import type { Schema } from "@/api/types";
import type { ColumnSpec } from "@/components/data-table/column-layout";
import type { DataTableColumn } from "@/components/data-table/data-table";
import { TimeText } from "@/components/time-text";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { BarCell } from "@/components/user/bar-cell";
import { Sparkline } from "@/components/user/sparkline";
import { formatCost, formatTokens } from "@/lib/format";
import { USER_COLUMN_IDS, type UserColumnId } from "@/lib/search/users";

const HIDDEN = new Set<UserColumnId>(["metadata"]);

const LABELS: Record<UserColumnId, string> = {
  user: "User",
  activity: "Activity",
  traces: "Traces",
  sessions: "Sessions",
  traces_per_session: "Traces / session",
  tokens: "Tokens",
  cost: "Cost",
  errors: "Errors",
  error_rate: "Error rate",
  metadata: "Metadata",
  first_seen: "First seen",
  last_seen: "Last seen",
};

export type UserColumn = ColumnSpec & { label: string };

export function userScoreNames(rows: readonly Schema<"EndUserSummary">[]): string[] {
  const names = new Set<string>();
  for (const row of rows) {
    for (const score of row.scores) {
      names.add(score.name);
    }
  }
  return [...names].sort((left, right) => left.localeCompare(right));
}

export function userColumnSpecs(scoreNames: readonly string[]): UserColumn[] {
  const fixed: UserColumn[] = USER_COLUMN_IDS.map((id) => ({
    id,
    label: LABELS[id],
    ...(HIDDEN.has(id) ? {} : { defaultVisible: true }),
  }));
  const scores: UserColumn[] = scoreNames.map((name) => ({
    id: `score:${name}`,
    label: name,
    defaultVisible: true,
  }));
  const at = fixed.findIndex((column) => column.id === "error_rate");
  return [...fixed.slice(0, at + 1), ...scores, ...fixed.slice(at + 1)];
}

export function orderedUserColumns(
  rows: readonly Schema<"EndUserSummary">[],
  visible: readonly string[],
): DataTableColumn<Schema<"EndUserSummary">>[] {
  const byId = new Map(userColumnDefs(rows).map((column) => [column.id, column]));
  return visible.flatMap((id) => {
    const column = byId.get(id);
    return column ? [column] : [];
  });
}

function maxOf(values: readonly number[]): number {
  return values.reduce((max, value) => Math.max(max, value), 0);
}

function userColumnDefs(
  rows: readonly Schema<"EndUserSummary">[],
): DataTableColumn<Schema<"EndUserSummary">>[] {
  const maxTraces = maxOf(rows.map((row) => row.trace_count));
  const maxTokens = maxOf(rows.map((row) => row.total_tokens ?? 0));
  const maxCost = maxOf(rows.map((row) => row.cost_total ?? 0));
  const fixed: DataTableColumn<Schema<"EndUserSummary">>[] = [
    {
      id: "user",
      size: 160,
      header: "User",
      enableSorting: false,
      meta: { mono: true },
      cell: ({ row }) => row.original.user_id,
    },
    {
      id: "activity",
      size: 96,
      header: "Activity",
      enableSorting: false,
      cell: ({ row }) => (
        <Sparkline
          values={row.original.activity}
          label={`${row.original.trace_count} traces over the range`}
        />
      ),
    },
    {
      id: "traces",
      accessorFn: (row) => row.trace_count,
      size: 72,
      header: "Traces",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => (
        <BarCell value={row.original.trace_count} max={maxTraces}>
          <span className="num">{row.original.trace_count}</span>
        </BarCell>
      ),
    },
    {
      id: "sessions",
      accessorFn: (row) => row.session_count,
      size: 88,
      header: "Sessions",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => <span className="num">{row.original.session_count}</span>,
    },
    {
      id: "traces_per_session",
      accessorFn: (row) => (row.session_count > 0 ? row.trace_count / row.session_count : null),
      size: 104,
      header: "Traces / session",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => (
        <span className="num">
          {row.original.session_count > 0
            ? (row.original.trace_count / row.original.session_count).toFixed(1)
            : "—"}
        </span>
      ),
    },
    {
      id: "tokens",
      accessorFn: (row) => row.total_tokens,
      size: 80,
      header: "Tokens",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => (
        <BarCell value={row.original.total_tokens} max={maxTokens}>
          <span className="num">{formatTokens(row.original.total_tokens)}</span>
        </BarCell>
      ),
    },
    {
      id: "cost",
      accessorFn: (row) => row.cost_total,
      size: 88,
      header: "Cost",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => (
        <BarCell value={row.original.cost_total} max={maxCost}>
          <span className="num">{formatCost(row.original.cost_total)}</span>
        </BarCell>
      ),
    },
    {
      id: "errors",
      accessorFn: (row) => row.error_count,
      size: 72,
      header: "Errors",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => (
        <span className={cn("num", row.original.error_count > 0 && "text-destructive")}>
          {row.original.error_count}
        </span>
      ),
    },
    {
      id: "error_rate",
      accessorFn: (row) => (row.trace_count > 0 ? row.error_trace_count / row.trace_count : null),
      size: 88,
      header: "Error rate",
      enableSorting: true,
      meta: { align: "right" },
      cell: ({ row }) => {
        const { trace_count: traces, error_trace_count: errors } = row.original;
        return (
          <span className={cn("num", errors > 0 && "text-destructive")}>
            {traces > 0 ? `${Math.round((errors / traces) * 100)}%` : "—"}
          </span>
        );
      },
    },
    {
      id: "metadata",
      size: 220,
      header: "Metadata",
      enableSorting: false,
      cell: ({ row }) => <MetadataCell metadata={row.original.metadata} />,
    },
    {
      id: "first_seen",
      accessorFn: (row) => row.first_seen,
      size: 168,
      header: "First seen",
      enableSorting: true,
      cell: ({ row }) => <TimeText value={row.original.first_seen} style="datetime" />,
    },
    {
      id: "last_seen",
      accessorFn: (row) => row.last_seen,
      size: 168,
      header: "Last seen",
      enableSorting: true,
      cell: ({ row }) => <TimeText value={row.original.last_seen} style="datetime" />,
    },
  ];
  const names = userScoreNames(rows);
  const scores = names.map((name) => scoreColumn(name));
  const at = fixed.findIndex((column) => column.id === "error_rate");
  return [...fixed.slice(0, at + 1), ...scores, ...fixed.slice(at + 1)];
}

function scoreColumn(name: string): DataTableColumn<Schema<"EndUserSummary">> {
  return {
    id: `score:${name}`,
    size: 96,
    header: name,
    enableSorting: false,
    meta: { align: "right" },
    cell: ({ row }) => <ScoreCell user={row.original} name={name} />,
  };
}

/** A user's average score: a percentage for booleans, two decimals for numbers. */
export function scoreText(score: Schema<"UserScore">): string {
  return score.data_type === "boolean" ? `${Math.round(score.avg * 100)}%` : score.avg.toFixed(2);
}

function ScoreCell({ user, name }: { user: Schema<"EndUserSummary">; name: string }) {
  const score = user.scores.find((item) => item.name === name);
  if (!score) {
    return <span className="text-muted-foreground">—</span>;
  }
  const text = scoreText(score);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="num">{text}</span>
      </TooltipTrigger>
      <TooltipContent>{`${score.count} scores`}</TooltipContent>
    </Tooltip>
  );
}

/** Stored metadata values are JSON-encoded; strings and numbers are shown decoded. */
export function decodeValue(raw: string): string {
  try {
    const value: unknown = JSON.parse(raw);
    if (typeof value === "string" || typeof value === "number") {
      return String(value);
    }
  } catch {
    // Not JSON: the stored text is shown as it is.
  }
  return raw;
}

function MetadataCell({ metadata }: { metadata?: Record<string, string> }) {
  const entries = Object.entries(metadata ?? {});
  if (entries.length === 0) {
    return <span className="text-muted-foreground">—</span>;
  }
  const text = entries.map(([key, value]) => `${key}: ${decodeValue(value)}`).join(" · ");
  return (
    <span className="block truncate" title={text}>
      {text}
    </span>
  );
}
