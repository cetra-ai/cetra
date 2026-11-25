import type { Schema } from "@/api/types";
import {
  type Dataset,
  type Mark,
  type MarkedSpan,
  type MarksContext,
  mix,
  type Profile,
  percentile,
  scoreUuid,
  spanId,
  visibleTraces,
} from "./dataset";
import { VOICE_PROFILE } from "./profiles";

type TraceSummary = Schema<"TraceSummary">;

const DAY_MS = 86_400_000;
const HOUR_MS = 3_600_000;
/** Salt of the availability ranks, so they differ from every other draw of the generator. */
const AVAILABILITY_SALT = 0x51ed270b;

/** The availability check's duration by quantile (ms), linear between the points (M17 P4). */
const DURATION_QUANTILES: readonly (readonly [number, number])[] = [
  [0, 400],
  [0.5, 1_200],
  [0.95, 3_000],
  [1, 4_000],
];

/** Whether the rank `r` (from 0) of a list is picked: `round(p × n)` of `n`, give or take one. */
function spread(p: number): (rank: number) => boolean {
  return (rank) => Math.floor((rank + 1) * p) > Math.floor(rank * p);
}

function quantile(u: number): number {
  for (let at = 1; at < DURATION_QUANTILES.length; at += 1) {
    const [u0, v0] = DURATION_QUANTILES[at - 1] ?? [0, 400];
    const [u1, v1] = DURATION_QUANTILES[at] ?? [1, 4_000];
    if (u <= u1) {
      return Math.round(v0 + ((u - u0) / (u1 - u0)) * (v1 - v0));
    }
  }
  return 4_000;
}

function durationOf(trace: TraceSummary): number {
  return trace.duration_ns / 1_000_000;
}

/** The interruption score every generated turn carries; the marks change some of them. */
function interruptionScores(index: number): Schema<"ScoreBrief">[] {
  return [
    {
      name: "interruption",
      score_id: scoreUuid(index, 0),
      source: "check",
      value: { data_type: "numeric", value: 0.9 },
    },
  ];
}

/** A turn of a call: its dataset index and its trace. */
type CallTurn = { index: number; trace: TraceSummary };

/** A call: the eligible traces of one session, in index order; `k` is the number in `call-NNNN`. */
type Call = { k: number; turns: CallTurn[] };

/** The calls among the eligible traces, by `k`. */
function callsOf(traces: readonly TraceSummary[], eligible: (index: number) => boolean): Call[] {
  const bySession = new Map<string, CallTurn[]>();
  traces.forEach((trace, index) => {
    const session = trace.session_id;
    if (session === undefined || !session.startsWith("call-") || !eligible(index)) {
      return;
    }
    bySession.set(session, [...(bySession.get(session) ?? []), { index, trace }]);
  });
  return [...bySession]
    .map(([session, turns]) => ({ k: Number.parseInt(session.slice("call-".length), 10), turns }))
    .sort((left, right) => left.k - right.k);
}

/** The availability check of a turn: a marked span that is the turn's last one (M17 P4). */
function availabilitySpan(durationMs: number): MarkedSpan {
  return {
    name: "check_availability",
    kind: "tool",
    fromMs: 250,
    durationMs,
    input: { format: "json", value: { day: "Friday", time: "morning" } },
    output: { format: "json", value: { slots: ["9:30", "11:00"] } },
  };
}

/** Adds a change to a turn's mark: tags and spans concatenate, scores too, the later fields win. */
function add(marks: Map<number, Mark>, index: number, change: Mark): void {
  const previous = marks.get(index);
  if (!previous) {
    marks.set(index, change);
    return;
  }
  const merged: Mark = { ...previous, ...change };
  const tags = [...(previous.tags ?? []), ...(change.tags ?? [])];
  const spans = [...(previous.spans ?? []), ...(change.spans ?? [])];
  const scores = [...(previous.scores ?? []), ...(change.scores ?? [])];
  marks.set(index, {
    ...merged,
    ...(tags.length > 0 ? { tags } : {}),
    ...(spans.length > 0 ? { spans } : {}),
    ...(scores.length > 0 ? { scores } : {}),
  });
}

/** The three voice marks (M17 P4): hang-ups after slow turns, availability checks, interruptions. */
function voiceMarks(
  traces: readonly TraceSummary[],
  context: MarksContext,
): ReadonlyMap<number, Mark> {
  const marks = new Map<number, Mark>();
  const calls = callsOf(traces, context.eligible);

  // 1. Hang-ups: the last turn of a call with k % 5 < 2 is tagged `hangup`. Of those calls, the turn before
  //    the last is slow (over 2.5 s) for 71% by the even spread, and an availability check in the slow ones.
  const touched = new Set<number>();
  const hangups = calls.filter((call) => call.turns.length >= 2 && call.k % 5 < 2);
  const slowRank = spread(0.71);
  hangups.forEach((call, rank) => {
    const last = call.turns[call.turns.length - 1];
    const before = call.turns[call.turns.length - 2];
    if (!last || !before) {
      return;
    }
    touched.add(last.index);
    touched.add(before.index);
    add(marks, last.index, { tags: ["hangup"] });
    if (slowRank(rank)) {
      const duration = 2_600 + (call.k % 12) * 100;
      add(marks, before.index, {
        tags: ["calendar"],
        spans: [availabilitySpan(duration)],
        durationMs: duration + 600,
      });
    } else {
      add(marks, before.index, { durationMs: Math.min(durationOf(before.trace), 2_400) });
    }
  });

  // 2. Availability checks: eligible turns not used by mark 1, with index % 10 < 3, ranked by a hash of
  //    their index; rank r of n takes the quantile (r + 0.5) / n.
  const pool: number[] = [];
  traces.forEach((_trace, index) => {
    if (context.eligible(index) && !touched.has(index) && index % 10 < 3) {
      pool.push(index);
    }
  });
  const keyOf = (index: number): number => mix(context.seed ^ AVAILABILITY_SALT, index);
  pool.sort((left, right) => keyOf(left) - keyOf(right) || left - right);
  pool.forEach((index, rank) => {
    const duration = quantile((rank + 0.5) / pool.length);
    add(marks, index, {
      tags: ["calendar"],
      spans: [availabilitySpan(duration)],
      durationMs: duration + 600,
    });
  });

  // 3. Interruptions: by the start day of a call's first turn (whole days before `now`). On day 0 the first
  //    round(0.06 × n) calls by k; on days 1, 3 and 5 the first call of the day. The middle turn is marked.
  const dayOf = (call: Call): number =>
    Math.floor((context.now - Date.parse(call.turns[0]?.trace.start_time ?? "")) / DAY_MS);
  const callsOnDay = (day: number): Call[] => calls.filter((call) => dayOf(call) === day);
  const dayZero = callsOnDay(0);
  const picked: Call[] = dayZero.slice(0, Math.round(0.06 * dayZero.length));
  for (const day of [1, 3, 5]) {
    const first = callsOnDay(day)[0];
    if (first) {
      picked.push(first);
    }
  }
  for (const call of picked) {
    const middle = call.turns[Math.floor(call.turns.length / 2)];
    if (middle) {
      add(marks, middle.index, {
        tags: ["interrupted"],
        scores: [
          {
            name: "interruption",
            source: "check",
            value: { data_type: "numeric", value: 0.3 },
          },
        ],
      });
    }
  }
  return marks;
}

/** The voice desk's story profile (M17 P4): the M16 voice profile with the hang-up, availability and interruption marks. */
export const VOICE_STORY_PROFILE: Profile = {
  ...VOICE_PROFILE,
  scores: interruptionScores,
  marks: voiceMarks,
};

export type VoiceFacts = {
  /** Median and 95th percentile of the availability checks (every marked `check_availability` span). */
  availabilityMedianMs: number;
  availabilityP95Ms: number;
  /** Share of hang-up calls whose turn before the last lasted over 2.5 s. */
  hangupsAfterSlowTurn: number;
  /** Among those slow turns, the share with a `calendar` turn whose availability check took 2 s or more. */
  slowTurnsWithAvailability: number;
  /** Share of the calls that started in the last 14 days with an `interrupted` turn. */
  interruptedCallShare: number;
  /** The p95 turn duration of each hour of the last 24 hours, oldest first. */
  hourlyP95Ms: number[];
  /** Every hourly p95 is above 1,500 ms. */
  p95AllDay: boolean;
};

/** The availability check of a turn: its last span, when it is a `check_availability` tool span, in ms. */
function availabilityMs(dataset: Dataset, turn: CallTurn): number | undefined {
  const detail = dataset.traceDetail(turn.trace.trace_id);
  const id = spanId(turn.index, turn.trace.span_count - 1);
  const span = detail?.spans.find((row) => row.span.span_id === id)?.span;
  if (span?.name !== "check_availability") {
    return undefined;
  }
  return span.duration_ns / 1_000_000;
}

/** The numbers the voice pictures quote, computed from a dataset built with `VOICE_STORY_PROFILE`. */
export function voiceFacts(dataset: Dataset): VoiceFacts {
  const showcases = new Set(VOICE_STORY_PROFILE.showcases.map((showcase) => showcase.index));
  const eligible = (index: number): boolean => {
    const trace = dataset.traces[index];
    return (
      trace !== undefined &&
      !showcases.has(index) &&
      visibleTraces([trace], false, false).length === 1
    );
  };
  const calls = callsOf(dataset.traces, eligible);

  const hangups = calls.filter(
    (call) =>
      call.turns.length >= 2 && call.turns[call.turns.length - 1]?.trace.tags.includes("hangup"),
  );
  const beforeLast = (call: Call): CallTurn => call.turns[call.turns.length - 2] as CallTurn;
  const slow = hangups.filter((call) => durationOf(beforeLast(call).trace) > 2_500);
  const slowWithAvailability = slow.filter((call) => {
    const turn = beforeLast(call);
    const check = availabilityMs(dataset, turn);
    return turn.trace.tags.includes("calendar") && check !== undefined && check >= 2_000;
  });

  const availability: number[] = [];
  dataset.traces.forEach((trace, index) => {
    if (!eligible(index) || !trace.tags.includes("calendar")) {
      return;
    }
    const check = availabilityMs(dataset, { index, trace });
    if (check !== undefined) {
      availability.push(check);
    }
  });

  const recent = calls.filter((call) => {
    const first = call.turns[0];
    const days = first
      ? Math.floor((dataset.now - Date.parse(first.trace.start_time)) / DAY_MS)
      : -1;
    return days >= 0 && days < 14;
  });
  const interrupted = recent.filter((call) =>
    call.turns.some((turn) => turn.trace.tags.includes("interrupted")),
  );

  const hourly: number[][] = Array.from({ length: 24 }, () => []);
  dataset.traces.forEach((trace, index) => {
    const age = dataset.now - Date.parse(trace.start_time);
    if (eligible(index) && age >= 0 && age < DAY_MS) {
      hourly[23 - Math.floor(age / HOUR_MS)]?.push(durationOf(trace));
    }
  });
  const hourlyP95Ms = hourly.map((values) => percentile(values, 95) ?? Number.NaN);

  return {
    availabilityMedianMs: percentile(availability, 50) ?? Number.NaN,
    availabilityP95Ms: percentile(availability, 95) ?? Number.NaN,
    hangupsAfterSlowTurn: slow.length / hangups.length,
    slowTurnsWithAvailability: slowWithAvailability.length / slow.length,
    interruptedCallShare: interrupted.length / recent.length,
    hourlyP95Ms,
    p95AllDay: hourlyP95Ms.every((value) => value > 1_500),
  };
}
