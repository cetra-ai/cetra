import { cn } from "cn";
import { Coins, DollarSign, ListTree, MessagesSquare, TriangleAlert } from "lucide-react";
import type { Schema } from "@/api/types";
import { ScoreAverageBadge } from "@/components/score-badge";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { categoryClasses } from "@/lib/colors";
import { formatCost, formatTokens } from "@/lib/format";
import { formatPercent } from "@/lib/metrics";
import { decodeValue, scoreText } from "@/lib/user-identity";

/** A user's totals as metric chips: traces, sessions, tokens, cost, error rate (UP-8). */
export function UserChips({ user }: { user: Schema<"EndUserSummary"> }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <MetricChip icon={ListTree} label="Traces">
        {count(user.trace_count, "trace")}
      </MetricChip>
      <MetricChip icon={MessagesSquare} label="Sessions">
        {count(user.session_count, "session")}
      </MetricChip>
      {user.total_tokens === undefined ? null : (
        <MetricChip icon={Coins} label="Tokens">
          {formatTokens(user.total_tokens)}
        </MetricChip>
      )}
      {user.cost_total === undefined ? null : (
        <MetricChip icon={DollarSign} label="Cost">
          {formatCost(user.cost_total)}
        </MetricChip>
      )}
      {user.trace_count > 0 ? (
        <MetricChip
          icon={TriangleAlert}
          label="Error rate"
          className={cn(user.error_trace_count > 0 && "border-status-error/40 text-status-error")}
        >
          {formatPercent(user.error_trace_count / user.trace_count)}
        </MetricChip>
      ) : null}
    </div>
  );
}

function count(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

/** The user's plan as a coloured pill. */
export function PlanPill({ plan }: { plan: string }) {
  return (
    <Badge variant="secondary" className={cn("h-4 px-1.5 text-[10px]", categoryClasses(plan).pill)}>
      {plan}
    </Badge>
  );
}

/** Score averages: name, badge, count; "No scores" when empty. */
export function UserScores({ scores }: { scores: readonly Schema<"UserScore">[] }) {
  if (scores.length === 0) {
    return <p className="text-muted-foreground">No scores</p>;
  }
  return (
    <ul className="flex flex-col gap-1">
      {scores.map((score) => (
        <li key={`${score.name}:${score.data_type}`} className="flex gap-2">
          <span className="min-w-0 truncate">{score.name}</span>
          <ScoreAverageBadge
            name={score.name}
            avg={score.avg}
            dataType={score.data_type}
            text={scoreText(score)}
            className="num"
          />
          <span className="num text-muted-foreground">{score.count}</span>
        </li>
      ))}
    </ul>
  );
}

/** The metadata as a key/value list; "No metadata" when empty. */
export function UserMetadata({ metadata }: { metadata: Record<string, string> | undefined }) {
  const entries = Object.entries(metadata ?? {});
  if (entries.length === 0) {
    return <p className="text-muted-foreground">No metadata</p>;
  }
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1">
      {entries.map(([key, value]) => (
        <div key={key} className="contents">
          <dt className="text-muted-foreground">{key}</dt>
          <dd className="min-w-0 break-words">{decodeValue(value)}</dd>
        </div>
      ))}
    </dl>
  );
}
