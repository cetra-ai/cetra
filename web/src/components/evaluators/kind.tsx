import { cn } from "cn";
import { ListChecks, type LucideIcon, Sparkles, Webhook } from "lucide-react";
import type { Schema } from "@/api/types";

type Kind = Schema<"EvaluatorKind">;

export const KIND_LABELS: Record<Kind, string> = {
  llm_judge: "LLM judge",
  check: "Check",
  webhook: "Webhook",
};

const KIND_ICONS: Record<Kind, { icon: LucideIcon; className: string }> = {
  llm_judge: { icon: Sparkles, className: "text-violet-500" },
  check: { icon: ListChecks, className: "text-emerald-500" },
  webhook: { icon: Webhook, className: "text-sky-500" },
};

/** The one config field that identifies the evaluator: the model, the check or the webhook host. */
export function detailOf(evaluator: Schema<"Evaluator">): string {
  const config: Record<string, unknown> = evaluator.config;
  const key =
    evaluator.kind === "llm_judge" ? "model" : evaluator.kind === "check" ? "check" : "url_host";
  const value = config[key];
  return typeof value === "string" ? value : "—";
}

/** The icon of an evaluator kind. Decorative: the kind's label sits beside it. */
export function KindIcon({ kind, className }: { kind: Kind; className?: string }) {
  const { icon: Icon, className: colour } = KIND_ICONS[kind];
  return (
    <Icon
      aria-hidden="true"
      data-kind={kind}
      className={cn("size-4 shrink-0", colour, className)}
    />
  );
}
