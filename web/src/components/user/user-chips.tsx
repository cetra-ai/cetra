import { cn } from "cn";
import { Coins, DollarSign, ListTree, MessagesSquare, TriangleAlert } from "lucide-react";
import type { Schema } from "@/api/types";
import { LazyTooltip } from "@/components/lazy-tooltip";
import { ScoreAverageBadge, useAverageGoodness } from "@/components/score-badge";
import { MetricChip } from "@/components/trace/metric-chip";
import { Badge } from "@/components/ui/badge";
import { categoryClasses } from "@/lib/colors";
import { formatCost, formatTokens } from "@/lib/format";
import { formatPercent } from "@/lib/metrics";
import { type ScoreTone, toneOf } from "@/lib/score-tone";
import { decodeValue, scoreText } from "@/lib/user-identity";

// Written out in full so Tailwind finds every class in this file.
const SESSIONS_TINT =
  "border-[color-mix(in_oklab,var(--chart-1)_40%,transparent)] [&>svg]:text-[var(--chart-1)]";
const TOKENS_TINT =
  "border-[color-mix(in_oklab,var(--chart-4)_40%,transparent)] [&>svg]:text-[var(--chart-4)]";
const COST_TINT =
  "border-[color-mix(in_oklab,var(--chart-2)_40%,transparent)] [&>svg]:text-[var(--chart-2)]";
const MUTED_COUNT = "[&_.num]:text-muted-foreground";

/**
 * A user's totals as metric chips: traces, sessions, tokens, cost, error rate (UP-8). With
 * `totalSessions`, the sessions chip also says how many sessions the user has in total.
 */
export function UserChips({
  user,
  totalSessions,
}: {
  user: Schema<"EndUserSummary">;
  totalSessions?: number;
}) {
  // A user with no sessions here, when the range has some, reads as muted: the tooltip says why.
  const sessionsMuted =
    user.session_count === 0 && totalSessions !== undefined && totalSessions > 0;
  const sessionsClass = cn(SESSIONS_TINT, sessionsMuted && MUTED_COUNT);
  const sessionsChip = (
    <MetricChip icon={MessagesSquare} label="Sessions" className={sessionsClass}>
      {count(user.session_count, "session")}
    </MetricChip>
  );
  return (
    <div className="flex flex-wrap items-center gap-2">
      <MetricChip icon={ListTree} label="Traces">
        {count(user.trace_count, "trace")}
      </MetricChip>
      {totalSessions === undefined ? (
        sessionsChip
      ) : (
        <LazyTooltip
          content={
            <span className="flex flex-col items-start">
              <span>{`${count(user.session_count, "session")} in this range`}</span>
              <span>{`${count(totalSessions, "session")} in total`}</span>
            </span>
          }
        >
          <MetricChip icon={MessagesSquare} label="Sessions" tabIndex={0} className={sessionsClass}>
            {count(user.session_count, "session")}
          </MetricChip>
        </LazyTooltip>
      )}
      {user.total_tokens === undefined ? null : (
        <MetricChip icon={Coins} label="Tokens" className={TOKENS_TINT}>
          {formatTokens(user.total_tokens)}
        </MetricChip>
      )}
      {user.cost_total === undefined ? null : (
        <MetricChip icon={DollarSign} label="Cost" className={COST_TINT}>
          {formatCost(user.cost_total)}
        </MetricChip>
      )}
      {user.trace_count > 0 ? (
        <MetricChip
          icon={TriangleAlert}
          label="Error rate"
          className={cn(
            user.error_trace_count > 0 &&
              "border-status-error/40 bg-status-error/10 text-status-error",
          )}
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
    <Badge
      variant="secondary"
      className={cn("h-4 px-1.5 text-caption", categoryClasses(plan).pill)}
    >
      {plan}
    </Badge>
  );
}

/** Solid bar colour per tone; the status tokens are the same ones the score badges use. */
const BAR_CLASSES: Record<ScoreTone, string> = {
  good: "bg-status-ok",
  mid: "bg-status-warn",
  bad: "bg-status-error",
};

/**
 * The fill of a score bar. A scored value fills by its goodness, coloured by its tone. Without a
 * direction, a boolean fills by its share of `true` and other scores have no fill.
 */
function fillOf(
  g: number | undefined,
  avg: number,
  dataType: Schema<"ScoreDataType">,
): { width: string; className: string } | undefined {
  if (g !== undefined) {
    return { width: `${Math.round(g * 100)}%`, className: BAR_CLASSES[toneOf(g)] };
  }
  if (dataType === "boolean") {
    return { width: `${Math.round(avg * 100)}%`, className: "bg-[var(--chart-1)]" };
  }
  return undefined;
}

/** A thin track with the score's fill. Decorative: the value badge next to it carries the number. */
function ScoreBar({
  name,
  avg,
  dataType,
}: {
  name: string;
  avg: number;
  dataType: Schema<"ScoreDataType">;
}) {
  const g = useAverageGoodness(name, avg, dataType);
  const fill = fillOf(g, avg, dataType);
  return (
    <span className="block h-1.5 overflow-hidden rounded-full bg-muted" aria-hidden="true">
      {fill ? (
        <span
          data-testid="score-fill"
          className={cn("block h-full rounded-full", fill.className)}
          style={{ width: fill.width }}
        />
      ) : null}
    </span>
  );
}

/** Score averages as rows: name, a bar, the value badge, the count; "No scores" when empty. */
export function UserScores({ scores }: { scores: readonly Schema<"UserScore">[] }) {
  if (scores.length === 0) {
    return <p className="text-muted-foreground">No scores</p>;
  }
  return (
    <ul className="flex flex-col gap-1.5">
      {scores.map((score) => (
        <li
          key={`${score.name}:${score.data_type}`}
          className="grid grid-cols-[minmax(96px,auto)_1fr_auto_auto] items-center gap-x-2 gap-y-1.5"
        >
          <span className="min-w-[96px] truncate">{score.name}</span>
          <ScoreBar name={score.name} avg={score.avg} dataType={score.data_type} />
          <ScoreAverageBadge
            name={score.name}
            avg={score.avg}
            dataType={score.data_type}
            text={scoreText(score)}
            className="num"
          />
          <span className="num text-caption text-muted-foreground">{score.count}</span>
        </li>
      ))}
    </ul>
  );
}

/** The flag emoji of a two-letter country code, from its regional indicator symbols. */
function flagOf(code: string): string {
  return String.fromCodePoint(...[...code].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65));
}

/** The metadata as pills of key and value; "No metadata" when empty. */
export function UserMetadata({ metadata }: { metadata: Record<string, string> | undefined }) {
  const entries = Object.entries(metadata ?? {});
  if (entries.length === 0) {
    return <p className="text-muted-foreground">No metadata</p>;
  }
  return (
    <ul className="flex flex-wrap gap-1.5">
      {entries.map(([key, value]) => {
        const decoded = decodeValue(value);
        const flag = key === "country" && /^[A-Z]{2}$/.test(decoded) ? flagOf(decoded) : undefined;
        return (
          <li
            key={key}
            className="inline-flex min-w-0 items-center gap-1 rounded-md border bg-surface px-1.5 py-0.5 text-body"
          >
            <span className="text-muted-foreground">{key}</span>
            {flag ? <span aria-hidden="true">{flag}</span> : null}
            <span className="max-w-[240px] truncate" title={decoded}>
              {decoded}
            </span>
          </li>
        );
      })}
    </ul>
  );
}
