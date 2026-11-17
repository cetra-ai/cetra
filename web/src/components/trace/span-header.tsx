import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { Clock, Coins, DollarSign, Sparkles, Timer, Zap } from "lucide-react";
import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { IdText } from "@/components/id-text";
import { KindIcon, kindLabel } from "@/components/kind-icon";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { environmentClasses } from "@/lib/colors";
import { formatCost, formatDuration, formatTokens } from "@/lib/format";

/**
 * The span detail, or the tree's span summary while the detail loads. The summary has the
 * identity, status, timing, tokens, cost, model and links; the detail-only fields (operation, the
 * cost breakdown, request and response model, conventions and truncation) appear with the detail.
 */
export type HeaderSpan = Schema<"SpanDetail"> | Schema<"SpanSummary">;

export function SpanHeader({ span, detached }: { span: HeaderSpan; detached: boolean }) {
  const tokens = tokenSummary(span);
  const { org, project } = useProject();
  const detail = "truncated" in span ? span : undefined;
  const modelsDiffer =
    detail !== undefined &&
    detail.request_model !== undefined &&
    detail.response_model !== undefined &&
    detail.request_model !== detail.response_model;

  return (
    <header className="flex shrink-0 flex-col gap-2 border-b px-4 py-3">
      <div className="flex min-w-0 items-center gap-2">
        <KindIcon kind={span.span_kind} />
        <div className="flex min-w-0 flex-col">
          <span className="text-[11px] uppercase tracking-wide text-muted-foreground">
            {span.parent_span_id === undefined ? "Root span" : kindLabel(span.span_kind)}
          </span>
          <span className="min-w-0 truncate text-[15px] font-semibold select-text">
            {span.name}
          </span>
        </div>
        {span.status === "ok" ? (
          <StatusPill className="bg-status-ok/12 text-status-ok">OK</StatusPill>
        ) : null}
        {span.status === "error" ? (
          <StatusPill className="bg-status-error/12 text-status-error">Error</StatusPill>
        ) : null}
        {span.status_message ? (
          <span className="min-w-0 truncate text-[12px] text-muted-foreground select-text">
            {span.status_message}
          </span>
        ) : null}
        <span className="ml-auto">
          <IdText value={span.span_id} kind="span" />
        </span>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <MetricChip icon={Clock} label="Start time">
          <TimeText value={span.start_time} style="time" />
        </MetricChip>
        <MetricChip icon={Timer} label="Duration">
          {formatDuration(span.duration_ns)}
        </MetricChip>
        {span.ttft_ns !== undefined ? (
          <MetricChip icon={Zap} label="Time to first token">
            {formatDuration(span.ttft_ns)}
          </MetricChip>
        ) : null}
        {tokens ? (
          <MetricChip icon={Coins} label="Tokens">
            {tokens}
          </MetricChip>
        ) : null}
        {span.cost_total !== undefined ? (
          detail ? (
            <Tooltip>
              <TooltipTrigger asChild>
                <MetricChip icon={DollarSign} label="Cost">
                  {formatCost(span.cost_total)}
                </MetricChip>
              </TooltipTrigger>
              <TooltipContent className="flex-col items-start">
                <span>input {formatCost(detail.cost_input)}</span>
                <span>output {formatCost(detail.cost_output)}</span>
                <span>{detail.cost_source ?? "none"}</span>
              </TooltipContent>
            </Tooltip>
          ) : (
            <MetricChip icon={DollarSign} label="Cost">
              {formatCost(span.cost_total)}
            </MetricChip>
          )
        ) : null}
        {span.model ? (
          modelsDiffer ? (
            <Tooltip>
              <TooltipTrigger asChild>
                <MetricChip icon={Sparkles} label="Model">
                  {span.model}
                </MetricChip>
              </TooltipTrigger>
              <TooltipContent className="flex-col items-start">
                <span>request {detail?.request_model}</span>
                <span>response {detail?.response_model}</span>
              </TooltipContent>
            </Tooltip>
          ) : (
            <MetricChip icon={Sparkles} label="Model">
              {span.model}
            </MetricChip>
          )
        ) : null}
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted-foreground">
        {span.provider ? <Pair label="provider">{span.provider}</Pair> : null}
        {detail?.operation ? <Pair label="operation">{detail.operation}</Pair> : null}
        {span.service_name ? <Pair label="service">{span.service_name}</Pair> : null}
        {span.environment ? (
          <span
            className={cn(
              "inline-flex rounded-sm border px-1.5 text-[11px]",
              environmentClasses(span.environment).pill,
            )}
          >
            {span.environment}
          </span>
        ) : null}
        {span.release ? <Pair label="release">{span.release}</Pair> : null}
        {span.session_id ? (
          <Pair label="session">
            <Link
              to="/$org/$project/sessions/$sessionId"
              params={{ org, project, sessionId: span.session_id }}
              className="focus-ring rounded-sm underline-offset-2 hover:underline"
            >
              {span.session_id}
            </Link>
          </Pair>
        ) : null}
        {span.user_id ? (
          <Pair label="user">
            <Link
              to="/$org/$project/users/$userId"
              params={{ org, project, userId: span.user_id }}
              className="focus-ring rounded-sm underline-offset-2 hover:underline"
            >
              {span.user_id}
            </Link>
          </Pair>
        ) : null}
        {detail && detail.truncated.length > 0 ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Badge variant="outline" className="h-4 px-1.5 text-[11px] text-muted-foreground">
                truncated at ingest
              </Badge>
            </TooltipTrigger>
            <TooltipContent>{detail.truncated.join(", ")}</TooltipContent>
          </Tooltip>
        ) : null}
        {detached ? (
          <Badge variant="secondary" className="h-4 px-1.5 text-[11px]">
            detached
          </Badge>
        ) : null}
        {detail?.conventions.map((convention) => (
          <Badge
            key={convention}
            variant="outline"
            className="h-4 px-1.5 text-[11px] text-muted-foreground"
          >
            {convention}
          </Badge>
        ))}
      </div>
    </header>
  );
}

function StatusPill({ className, children }: { className: string; children: ReactNode }) {
  return (
    <span className={cn("rounded px-1.5 text-[11px] font-medium", className)}>{children}</span>
  );
}

function Pair({ label, children }: { label: string; children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1">
      <span>{label}</span>
      <span className="text-foreground">{children}</span>
    </span>
  );
}

/** `in 1.2k · out 40 · cache 1.0k`, with a part only when that count is present. */
function tokenSummary(span: HeaderSpan): string | undefined {
  const parts: string[] = [];
  if (span.input_tokens !== undefined) {
    parts.push(`in ${formatTokens(span.input_tokens)}`);
  }
  if (span.output_tokens !== undefined) {
    parts.push(`out ${formatTokens(span.output_tokens)}`);
  }
  if (span.cache_read_tokens !== undefined) {
    parts.push(`cache ${formatTokens(span.cache_read_tokens)}`);
  }
  if (span.cache_write_tokens !== undefined) {
    parts.push(`cache write ${formatTokens(span.cache_write_tokens)}`);
  }
  if (span.reasoning_tokens !== undefined) {
    parts.push(`reasoning ${formatTokens(span.reasoning_tokens)}`);
  }
  return parts.length > 0 ? parts.join(" · ") : undefined;
}
