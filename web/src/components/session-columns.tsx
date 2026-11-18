import { cn } from "cn";
import {
  Clock,
  Coins,
  DollarSign,
  Gauge,
  ListTree,
  MessagesSquare,
  Tag,
  Timer,
  TriangleAlert,
  User,
} from "lucide-react";
import type { Schema } from "@/api/types";
import type { ColumnSpec } from "@/components/data-table/column-layout";
import type { DataTableColumn } from "@/components/data-table/data-table";
import { filterIdCell } from "@/components/filter-hit";
import { LazyTooltip } from "@/components/lazy-tooltip";
import { RelativeTime } from "@/components/relative-time";
import { ScoreBadge } from "@/components/score-badge";
import { TimeText } from "@/components/time-text";
import { formatScore, Preview } from "@/components/trace-columns";
import { Badge } from "@/components/ui/badge";
import { BarCell } from "@/components/user/bar-cell";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { SESSION_COLUMN_IDS, type SessionColumnId } from "@/lib/search/sessions";

const HIDDEN = new Set<SessionColumnId>(["tags", "last_answer"]);

const LABELS: Record<SessionColumnId, string> = {
  session: "Session",
  first_message: "First message",
  start: "Start",
  last: "Last activity",
  traces: "Traces",
  user: "User",
  duration: "Duration",
  tokens: "Tokens",
  cost: "Cost",
  errors: "Errors",
  error_rate: "Error rate",
  last_answer: "Last answer",
  tags: "Tags",
};

const SOURCE_LABEL: Record<Schema<"ScoreSource">, string> = {
  api: "API",
  sdk: "SDK",
  human: "Human",
  end_user: "End user",
  llm_judge: "LLM judge",
  check: "Check",
  webhook: "Webhook",
};

export type SessionColumn = ColumnSpec & { label: string };

export function sessionScoreNames(rows: readonly Schema<"SessionSummary">[]): string[] {
  const names = new Set<string>();
  for (const row of rows) {
    for (const score of row.scores) {
      names.add(score.name);
    }
  }
  return [...names].sort((left, right) => left.localeCompare(right));
}

export function sessionColumnSpecs(names: readonly string[]): SessionColumn[] {
  const fixed = SESSION_COLUMN_IDS.map((id) => ({
    id,
    label: LABELS[id],
    ...(!HIDDEN.has(id) ? { defaultVisible: true } : {}),
  }));
  const scores: SessionColumn[] = names.map((name) => ({
    id: `score:${name}`,
    label: name,
    defaultVisible: true,
  }));
  return [...fixed, ...scores];
}

export function orderedSessionColumns(
  rows: readonly Schema<"SessionSummary">[],
  visible: readonly string[],
  onFilter?: (conjunct: string) => void,
): DataTableColumn<Schema<"SessionSummary">>[] {
  const byId = new Map(sessionColumnDefs(rows, onFilter).map((column) => [column.id, column]));
  return visible.flatMap((id) => {
    const column = byId.get(id);
    return column ? [column] : [];
  });
}

function maxOf(values: readonly number[]): number {
  return values.reduce((max, value) => Math.max(max, value), 0);
}

function sessionColumnDefs(
  rows: readonly Schema<"SessionSummary">[],
  onFilter?: (conjunct: string) => void,
): DataTableColumn<Schema<"SessionSummary">>[] {
  const maxTraces = maxOf(rows.map((row) => row.trace_count));
  const maxTokens = maxOf(rows.map((row) => row.total_tokens ?? 0));
  const maxCost = maxOf(rows.map((row) => row.cost_total ?? 0));
  const fixed: DataTableColumn<Schema<"SessionSummary">>[] = [
    {
      id: "session",
      size: 160,
      header: "Session",
      enableSorting: false,
      meta: { mono: true, icon: MessagesSquare },
      cell: ({ row }) => row.original.session_id,
    },
    {
      id: "first_message",
      size: 360,
      header: "First message",
      enableSorting: false,
      meta: { grow: true, icon: MessagesSquare },
      cell: ({ row }) => <Preview text={row.original.first_input_preview} />,
    },
    {
      id: "start",
      accessorFn: (row) => row.start_time,
      size: 168,
      header: "Start",
      meta: { icon: Clock },
      enableSorting: true,
      cell: ({ row }) => <TimeText value={row.original.start_time} style="datetime" />,
    },
    {
      id: "last",
      accessorFn: (row) => row.end_time,
      size: 168,
      header: "Last activity",
      meta: { icon: Clock },
      enableSorting: true,
      cell: ({ row }) => <RelativeTime value={row.original.end_time} />,
    },
    {
      id: "traces",
      accessorFn: (row) => row.trace_count,
      size: 72,
      header: "Traces",
      enableSorting: true,
      meta: { align: "right", icon: ListTree },
      cell: ({ row }) => (
        <BarCell value={row.original.trace_count} max={maxTraces}>
          <span className="num">{row.original.trace_count}</span>
        </BarCell>
      ),
    },
    {
      id: "user",
      size: 140,
      header: "User",
      enableSorting: false,
      meta: { mono: true, icon: User },
      cell: ({ row }) => filterIdCell(row.original.user_id, "user", "user_id", "user", onFilter),
    },
    {
      id: "duration",
      accessorFn: (row) => row.duration_ns,
      size: 88,
      header: "Duration",
      enableSorting: false,
      meta: { align: "right", icon: Timer },
      cell: ({ row }) => <span className="num">{formatDuration(row.original.duration_ns)}</span>,
    },
    {
      id: "tokens",
      accessorFn: (row) => row.total_tokens,
      size: 80,
      header: "Tokens",
      enableSorting: false,
      meta: { align: "right", icon: Coins },
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
      meta: { align: "right", icon: DollarSign },
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
      enableSorting: false,
      meta: { align: "right", icon: TriangleAlert },
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
      enableSorting: false,
      meta: { align: "right", icon: TriangleAlert },
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
      id: "last_answer",
      size: 360,
      header: "Last answer",
      enableSorting: false,
      meta: { icon: MessagesSquare },
      cell: ({ row }) => <Preview text={row.original.last_output_preview} />,
    },
    {
      id: "tags",
      size: 140,
      header: "Tags",
      enableSorting: false,
      meta: { icon: Tag },
      cell: ({ row }) => <Tags tags={row.original.tags} />,
    },
  ];
  const names = sessionScoreNames(rows);
  return [...fixed, ...names.map((name) => scoreColumn(name))];
}

function scoreColumn(name: string): DataTableColumn<Schema<"SessionSummary">> {
  return {
    id: `score:${name}`,
    size: 96,
    header: name,
    enableSorting: false,
    meta: { icon: Gauge },
    cell: ({ row }) => <ScoreCell session={row.original} name={name} />,
  };
}

function ScoreCell({ session, name }: { session: Schema<"SessionSummary">; name: string }) {
  const score = session.scores.find((item) => item.name === name);
  if (!score) {
    return <span className="text-muted-foreground">—</span>;
  }
  const numeric = score.value.data_type === "numeric" || score.value.data_type === "boolean";
  return (
    <LazyTooltip content={SOURCE_LABEL[score.source]}>
      <ScoreBadge
        name={name}
        value={score.value}
        text={formatScore(score.value)}
        className={numeric ? "num" : "block truncate"}
      />
    </LazyTooltip>
  );
}

function Tags({ tags }: { tags: string[] }) {
  if (tags.length === 0) {
    return "—";
  }
  const shown = tags.slice(0, 2);
  const rest = tags.length - shown.length;
  return (
    <span className="flex min-w-0 items-center gap-1">
      {shown.map((tag) => (
        <Badge key={tag} variant="secondary" className="h-4 max-w-24 truncate px-1.5 text-caption">
          {tag}
        </Badge>
      ))}
      {rest > 0 ? <span className="text-caption text-muted-foreground">+{rest}</span> : null}
    </span>
  );
}
