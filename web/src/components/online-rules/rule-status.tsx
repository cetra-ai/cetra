import { cn } from "cn";
import type { Schema } from "@/api/types";
import { TimeText } from "@/components/time-text";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { pausedMessage } from "./rule-banner";
import { ruleStatusLabel } from "./rule-form";

type Rule = Schema<"OnlineRule">;

export type RuleBucket = "active" | "paused" | "disabled";
export const RULE_BUCKETS: readonly RuleBucket[] = ["active", "paused", "disabled"];

export function ruleBucket(rule: Pick<Rule, "enabled" | "paused_reason">): RuleBucket {
  if (!rule.enabled) {
    return "disabled";
  }
  return rule.paused_reason !== undefined ? "paused" : "active";
}

const PILL: Record<"ok" | "warn" | "error" | "unset", { dot: string; pill: string }> = {
  ok: { dot: "bg-status-ok", pill: "border border-status-ok/30 bg-status-ok/10 text-status-ok" },
  warn: {
    dot: "bg-status-warn",
    pill: "border border-status-warn/30 bg-status-warn/10 text-status-warn",
  },
  error: {
    dot: "bg-status-error",
    pill: "border border-status-error/30 bg-status-error/10 text-status-error",
  },
  unset: { dot: "bg-status-unset", pill: "border text-muted-foreground" },
};

function pillTone(rule: Rule): keyof typeof PILL {
  switch (ruleBucket(rule)) {
    case "active":
      return "ok";
    case "disabled":
      return "unset";
    case "paused":
      return rule.paused_reason === "budget" ? "warn" : "error";
  }
}

/** The rule's status as a pill; a paused rule explains its pause on hover. The caller provides TooltipProvider. */
export function RuleStatusPill({ rule }: { rule: Rule }) {
  const tone = PILL[pillTone(rule)];
  const pill = (
    <span
      className={cn(
        "inline-flex w-fit items-center gap-1.5 rounded-full px-2 py-0.5 text-caption font-medium",
        tone.pill,
      )}
    >
      <span aria-hidden="true" className={cn("size-1.5 shrink-0 rounded-full", tone.dot)} />
      {ruleStatusLabel(rule)}
    </span>
  );
  const message = pausedMessage(rule, undefined);
  if (message === undefined) {
    return pill;
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>{pill}</TooltipTrigger>
      <TooltipContent className="flex-col items-start">
        <span>{message}</span>
        {rule.paused_at ? (
          <span>
            Since <TimeText value={rule.paused_at} style="datetime" />
          </span>
        ) : null}
      </TooltipContent>
    </Tooltip>
  );
}
