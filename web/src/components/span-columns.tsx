import { cn } from "cn";
import {
  Clock,
  Coins,
  DollarSign,
  FileText,
  Layers,
  MessageSquare,
  MessageSquareText,
  MessagesSquare,
  Sparkles,
  Tag,
  Timer,
  User,
} from "lucide-react";
import type { Schema } from "@/api/types";
import type { ColumnSpec } from "@/components/data-table/column-layout";
import type { DataTableColumn } from "@/components/data-table/data-table";
import { FilterHit, filterIdCell } from "@/components/filter-hit";
import { KindIcon } from "@/components/kind-icon";
import { LazyTooltip } from "@/components/lazy-tooltip";
import { StatusDot } from "@/components/status-dot";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { categoryClasses, environmentClasses, providerClasses } from "@/lib/colors";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";
import { SPAN_COLUMN_IDS, type SpanColumnId } from "@/lib/search/spans";

const TAGS_SHOWN = 2;

const HIDDEN = new Set<SpanColumnId>([
  "prompt",
  "provider",
  "service",
  "release",
  "tags",
  "trace_id",
  "span_id",
  "parent_span_id",
]);
const LOCKED = new Set<SpanColumnId>(["status", "name"]);

const LABELS: Record<SpanColumnId, string> = {
  status: "Status",
  start: "Start",
  kind: "Kind",
  name: "Name",
  model: "Model",
  prompt: "Prompt",
  input: "Input",
  output: "Output",
  duration: "Duration",
  ttft: "TTFT",
  tokens: "Tokens",
  cost: "Cost",
  session: "Session",
  user: "User",
  env: "Env",
  provider: "Provider",
  service: "Service",
  release: "Release",
  tags: "Tags",
  trace_id: "Trace ID",
  span_id: "Span ID",
  parent_span_id: "Parent span ID",
};

const KIND_LABEL: Record<Schema<"SpanKind">, string> = {
  llm: "LLM",
  embedding: "Embedding",
  tool: "Tool",
  agent: "Agent",
  chain: "Chain",
  retriever: "Retriever",
  reranker: "Reranker",
  guardrail: "Guardrail",
  evaluator: "Evaluator",
  span: "Span",
};

export type SpanColumn = ColumnSpec & { label: string };

export function spanColumnSpecs(): SpanColumn[] {
  return SPAN_COLUMN_IDS.map((id) => ({
    id,
    label: LABELS[id],
    ...(LOCKED.has(id) ? { locked: true } : {}),
    ...(!HIDDEN.has(id) ? { defaultVisible: true } : {}),
  }));
}

/** Failed spans get a red edge, unless they are selected (the selected style wins). */
export function spanRowTone(row: Schema<"SpanSummary">) {
  return row.status === "error" ? ("error" as const) : undefined;
}

export function orderedSpanColumns(
  visible: readonly string[],
  onFilter?: (conjunct: string) => void,
): DataTableColumn<Schema<"SpanSummary">>[] {
  const byId = new Map(spanColumnDefs(onFilter).map((column) => [column.id, column]));
  return visible.flatMap((id) => {
    const column = byId.get(id);
    return column ? [column] : [];
  });
}

function spanColumnDefs(
  onFilter?: (conjunct: string) => void,
): DataTableColumn<Schema<"SpanSummary">>[] {
  return [
    {
      id: "status",
      size: 28,
      header: () => <span className="sr-only">Status</span>,
      cell: ({ row }) => (
        <FilterHit
          label={`Filter by status ${row.original.status}`}
          field={{ column: "status" }}
          value={row.original.status}
          onFilter={onFilter}
        >
          <StatusDot status={row.original.status} />
        </FilterHit>
      ),
    },
    {
      id: "start",
      accessorFn: (row) => row.start_time,
      size: 132,
      header: "Start",
      meta: { icon: Clock },
      enableSorting: true,
      cell: ({ row }) => <TimeText value={row.original.start_time} style="datetime" />,
    },
    {
      id: "kind",
      size: 104,
      header: "Kind",
      cell: ({ row }) => (
        <FilterHit
          label={`Filter by kind ${row.original.span_kind}`}
          field={{ column: "span_kind" }}
          value={row.original.span_kind}
          onFilter={onFilter}
        >
          <span className="flex min-w-0 items-center gap-1">
            <KindIcon kind={row.original.span_kind} />
            <span className="truncate">{KIND_LABEL[row.original.span_kind]}</span>
          </span>
        </FilterHit>
      ),
    },
    {
      id: "name",
      size: 200,
      header: "Name",
      cell: ({ row }) => (
        <FilterHit
          label={`Filter by name ${row.original.name}`}
          field={{ column: "name" }}
          value={row.original.name}
          onFilter={onFilter}
        >
          {row.original.name}
        </FilterHit>
      ),
    },
    {
      id: "model",
      size: 140,
      header: "Model",
      meta: { icon: Sparkles },
      cell: ({ row }) => {
        const model = row.original.model;
        if (!model) {
          return "—";
        }
        return (
          <FilterHit
            label={`Filter by model ${model}`}
            field={{ column: "model" }}
            value={model}
            onFilter={onFilter}
          >
            <span className="flex min-w-0 items-center gap-1.5">
              <span
                aria-hidden
                className={cn("size-2 shrink-0 rounded-full", providerClasses(model).dot)}
              />
              <span className="truncate">{model}</span>
            </span>
          </FilterHit>
        );
      },
    },
    {
      id: "prompt",
      size: 140,
      header: "Prompt",
      meta: { icon: FileText },
      cell: ({ row }) => {
        const name = row.original.prompt_name;
        if (!name) {
          return "—";
        }
        const version = row.original.prompt_version;
        return (
          <FilterHit
            label={`Filter by prompt ${name}`}
            field={{ column: "prompt_name" }}
            value={name}
            onFilter={onFilter}
          >
            {version ? `${name}@${version}` : name}
          </FilterHit>
        );
      },
    },
    {
      id: "input",
      size: 280,
      header: "Input",
      meta: { icon: MessageSquare },
      cell: ({ row }) => <Preview text={row.original.input_preview} />,
    },
    {
      id: "output",
      size: 280,
      header: "Output",
      meta: { icon: MessageSquareText },
      cell: ({ row }) => <Preview text={row.original.output_preview} />,
    },
    {
      id: "duration",
      accessorFn: (row) => row.duration_ns,
      size: 80,
      header: "Duration",
      enableSorting: true,
      meta: { align: "right", icon: Timer },
      cell: ({ row }) => <span className="num">{formatDuration(row.original.duration_ns)}</span>,
    },
    {
      id: "ttft",
      accessorFn: (row) => row.ttft_ns,
      size: 72,
      header: "TTFT",
      meta: { align: "right" },
      cell: ({ row }) => <span className="num">{formatDurationOrDash(row.original.ttft_ns)}</span>,
    },
    {
      id: "tokens",
      accessorFn: (row) => row.total_tokens,
      size: 72,
      header: "Tokens",
      enableSorting: true,
      meta: { align: "right", icon: Coins },
      cell: ({ row }) => <TokenCount span={row.original} />,
    },
    {
      id: "cost",
      accessorFn: (row) => row.cost_total,
      size: 80,
      header: "Cost",
      enableSorting: true,
      meta: { align: "right", icon: DollarSign },
      cell: ({ row }) => <span className="num">{formatCost(row.original.cost_total)}</span>,
    },
    {
      id: "session",
      size: 120,
      header: "Session",
      meta: { mono: true, icon: MessagesSquare },
      cell: ({ row }) =>
        filterIdCell(row.original.session_id, "session", "session_id", "session", onFilter),
    },
    {
      id: "user",
      size: 100,
      header: "User",
      meta: { mono: true, icon: User },
      cell: ({ row }) => filterIdCell(row.original.user_id, "user", "user_id", "user", onFilter),
    },
    {
      id: "env",
      size: 112,
      header: "Env",
      meta: { icon: Layers },
      cell: ({ row }) => {
        const env = row.original.environment;
        if (!env) {
          return "—";
        }
        return (
          <span
            className={cn(
              "inline-flex rounded-sm border px-1.5 text-caption",
              environmentClasses(env).pill,
            )}
          >
            {env}
          </span>
        );
      },
    },
    {
      id: "provider",
      size: 100,
      header: "Provider",
      cell: ({ row }) =>
        row.original.provider ? (
          <FilterHit
            label={`Filter by provider ${row.original.provider}`}
            field={{ column: "provider" }}
            value={row.original.provider}
            onFilter={onFilter}
          >
            {row.original.provider}
          </FilterHit>
        ) : (
          "—"
        ),
    },
    {
      id: "service",
      size: 120,
      header: "Service",
      cell: ({ row }) => row.original.service_name ?? "—",
    },
    {
      id: "release",
      size: 88,
      header: "Release",
      cell: ({ row }) => row.original.release ?? "—",
    },
    {
      id: "tags",
      size: 140,
      header: "Tags",
      meta: { icon: Tag },
      cell: ({ row }) => <Tags tags={row.original.tags} />,
    },
    {
      id: "trace_id",
      size: 220,
      header: "Trace ID",
      meta: { mono: true },
      cell: ({ row }) => row.original.trace_id,
    },
    {
      id: "span_id",
      size: 140,
      header: "Span ID",
      meta: { mono: true },
      cell: ({ row }) => row.original.span_id,
    },
    {
      id: "parent_span_id",
      size: 140,
      header: "Parent span ID",
      meta: { mono: true },
      cell: ({ row }) => row.original.parent_span_id ?? "—",
    },
  ];
}

function formatDurationOrDash(ns: number | undefined): string {
  if (ns === undefined) {
    return "—";
  }
  return formatDuration(ns);
}

function Preview({ text }: { text?: string }) {
  if (!text) {
    return <span className="text-muted-foreground">—</span>;
  }
  return (
    <LazyTooltip content={text}>
      <span className="block truncate text-muted-foreground">{text}</span>
    </LazyTooltip>
  );
}

function TokenCount({ span }: { span: Schema<"SpanSummary"> }) {
  return (
    <LazyTooltip
      content={
        <span className="flex flex-col items-start">
          <span>Input {formatTokens(span.input_tokens)}</span>
          <span>Output {formatTokens(span.output_tokens)}</span>
          <span>Cache read {formatTokens(span.cache_read_tokens)}</span>
          <span>Cache write {formatTokens(span.cache_write_tokens)}</span>
          <span>Reasoning {formatTokens(span.reasoning_tokens)}</span>
        </span>
      }
    >
      <span className="num">{formatTokens(span.total_tokens)}</span>
    </LazyTooltip>
  );
}

function Tags({ tags }: { tags: string[] }) {
  if (tags.length === 0) {
    return "—";
  }
  const shown = tags.slice(0, TAGS_SHOWN);
  const rest = tags.length - shown.length;
  return (
    <span className="flex min-w-0 items-center gap-1">
      {shown.map((tag) => (
        <Badge
          key={tag}
          variant="secondary"
          className={cn("h-4 px-1.5 text-caption", categoryClasses(tag).pill)}
        >
          {tag}
        </Badge>
      ))}
      {rest > 0 ? <span className="text-muted-foreground">+{rest}</span> : null}
    </span>
  );
}
