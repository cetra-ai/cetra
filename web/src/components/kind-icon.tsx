import {
  IconBinary,
  IconCircle,
  IconGauge,
  IconLink,
  IconListNumbers,
  IconRobot,
  IconSearch,
  IconShieldCheck,
  IconSparkles,
  IconTool,
  type Icon as TablerIcon,
} from "@tabler/icons-react";
import type { Schema } from "@/api/types";

// Tabler's outline icons, 14px with a slightly lighter stroke than their default 2.
const KIND_ICON_PX = 14;
const KIND_ICON_STROKE = 1.75;

const KINDS = {
  llm: { icon: IconSparkles, label: "LLM", className: "text-kind-llm" },
  embedding: { icon: IconBinary, label: "Embedding", className: "text-kind-embedding" },
  tool: { icon: IconTool, label: "Tool", className: "text-kind-tool" },
  agent: { icon: IconRobot, label: "Agent", className: "text-kind-agent" },
  chain: { icon: IconLink, label: "Chain", className: "text-kind-chain" },
  retriever: { icon: IconSearch, label: "Retriever", className: "text-kind-retriever" },
  reranker: { icon: IconListNumbers, label: "Reranker", className: "text-kind-reranker" },
  guardrail: { icon: IconShieldCheck, label: "Guardrail", className: "text-kind-guardrail" },
  evaluator: { icon: IconGauge, label: "Evaluator", className: "text-kind-evaluator" },
  span: { icon: IconCircle, label: "Span", className: "text-kind-span" },
} as const satisfies Record<
  Schema<"SpanKind">,
  { icon: TablerIcon; label: string; className: string }
>;

/** The human label for a span kind, e.g. `LLM` for `llm`. Unknown kinds show the raw value. */
export function kindLabel(kind: Schema<"SpanKind">): string {
  return KINDS[kind]?.label ?? kind;
}

export function KindIcon({ kind }: { kind: Schema<"SpanKind"> }) {
  const entry = KINDS[kind] ?? { icon: IconCircle, label: kind, className: "text-kind-span" };
  const Icon = entry.icon;
  return (
    <Icon
      aria-label={entry.label}
      size={KIND_ICON_PX}
      stroke={KIND_ICON_STROKE}
      className={`shrink-0 ${entry.className}`}
    />
  );
}
