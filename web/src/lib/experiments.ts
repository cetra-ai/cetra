import type { Schema } from "@/api/types";

export const DEFAULT_THRESHOLD = 0.1;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_COMPARE_IDS = 4;
const MAX_THRESHOLD = 1000;
const MAX_LABELS = 4;
const MAX_TEXT_CHARS = 40;
const MINUS = "−";

function stringParam(search: Record<string, unknown>, key: string): string | undefined {
  const value = search[key];
  return typeof value === "string" ? value.trim() : undefined;
}

function isFlag(value: unknown): boolean {
  return value === "1" || value === true;
}

/** `?dataset=` of the list: a UUID string, else undefined. */
export function parseExperimentsSearch(search: Record<string, unknown>): { dataset?: string } {
  const dataset = stringParam(search, "dataset");
  return dataset && UUID.test(dataset) ? { dataset } : {};
}

/** The ids of a compare search (`parseCompareSearch().ids`) as a list, in URL order. */
export function compareIdList(ids: string): string[] {
  return ids === "" ? [] : ids.split(",");
}

/** `?errors=1` of the experiment page. */
export function parseExperimentSearch(search: Record<string, unknown>): { errors?: 1 } {
  return isFlag(search.errors) ? { errors: 1 } : {};
}

/**
 * `?ids=a,b&threshold=0.2&regressions=1` of the compare view. `ids`: split on commas, trimmed,
 * empty parts dropped, duplicates dropped (first kept), UUID strings only, at most 4 kept, and
 * joined back with commas (the URL keeps the string; `compareIdList` splits it). `threshold`: a
 * finite number in [0, 1000], else undefined (the view uses DEFAULT_THRESHOLD).
 */
export function parseCompareSearch(search: Record<string, unknown>): {
  ids: string;
  threshold?: number;
  regressions?: 1;
} {
  const ids: string[] = [];
  for (const part of (stringParam(search, "ids") ?? "").split(",")) {
    const id = part.trim();
    if (id && UUID.test(id) && !ids.includes(id) && ids.length < MAX_COMPARE_IDS) {
      ids.push(id);
    }
  }
  const raw = stringParam(search, "threshold");
  const value = raw ? Number(raw) : Number.NaN;
  const threshold =
    Number.isFinite(value) && value >= 0 && value <= MAX_THRESHOLD ? value : undefined;
  return {
    ids: ids.join(","),
    ...(threshold !== undefined ? { threshold } : {}),
    ...(isFlag(search.regressions) ? { regressions: 1 as const } : {}),
  };
}

/** A fraction as a percentage with at most one decimal and no trailing ".0". */
function percentText(fraction: number): string {
  return String(Math.round(fraction * 1000) / 10);
}

/** One line for a score summary (D-J-13). Absent statistics are left out. */
export function scoreSummaryText(summary: Schema<"ScoreSummary">): string {
  const count = `n ${summary.count}`;
  switch (summary.data_type) {
    case "numeric": {
      const parts: string[] = [];
      if (summary.mean !== undefined) {
        parts.push(`mean ${summary.mean.toFixed(3)}`);
      }
      if (summary.median !== undefined) {
        parts.push(`median ${summary.median.toFixed(3)}`);
      }
      parts.push(count);
      return parts.join(" · ");
    }
    case "boolean": {
      const parts: string[] = [];
      if (summary.pass_rate !== undefined) {
        parts.push(`pass rate ${percentText(summary.pass_rate)} %`);
      }
      parts.push(count);
      return parts.join(" · ");
    }
    case "categorical": {
      const labels = Object.entries(summary.labels).sort(
        ([nameA, countA], [nameB, countB]) => countB - countA || nameA.localeCompare(nameB),
      );
      const parts = labels
        .slice(0, MAX_LABELS)
        .map(([label, labelCount]) => `${label} ${labelCount}`);
      if (labels.length > MAX_LABELS) {
        parts.push(`+${labels.length - MAX_LABELS} more`);
      }
      return parts.join(" · ");
    }
    case "text":
      return count;
  }
}

/**
 * A delta as "+0.120" / "−0.050" (U+2212) / "0.000", three decimals; a boolean score's delta
 * in points: "+12.5 pts".
 */
export function deltaText(delta: number, dataType: Schema<"ScoreSummary">["data_type"]): string {
  const scale = dataType === "boolean" ? 100 : 1;
  const digits = dataType === "boolean" ? 1 : 3;
  const magnitude = Number((Math.abs(delta) * scale).toFixed(digits));
  const sign = magnitude === 0 ? "" : delta > 0 ? "+" : MINUS;
  const number = magnitude.toFixed(digits);
  return dataType === "boolean" ? `${sign}${number} pts` : `${sign}${number}`;
}

/** "better" when delta > 0, "worse" when delta < 0, "same" at 0 (CG-J1: higher is better). */
export function deltaDirection(delta: number): "better" | "worse" | "same" {
  if (delta > 0) {
    return "better";
  }
  if (delta < 0) {
    return "worse";
  }
  return "same";
}

/**
 * A score value of a result row: numeric with 3 decimals, boolean "pass" (1) / "fail" (0),
 * categorical its label, text its first 40 characters with "…"; "—" when the value it needs
 * is absent.
 */
export function scoreValueText(value: Schema<"ResultScore">): string {
  switch (value.data_type) {
    case "numeric":
      return value.value_num === undefined ? "—" : value.value_num.toFixed(3);
    case "boolean":
      if (value.value_num === 1) {
        return "pass";
      }
      if (value.value_num === 0) {
        return "fail";
      }
      return "—";
    case "categorical":
      return value.value_str ?? "—";
    case "text": {
      if (value.value_str === undefined) {
        return "—";
      }
      const chars = [...value.value_str];
      return chars.length > MAX_TEXT_CHARS
        ? `${chars.slice(0, MAX_TEXT_CHARS).join("")}…`
        : value.value_str;
    }
  }
}

/** Runs expected: item_count × repetitions. */
export function expectedRuns(experiment: Schema<"Experiment">): number {
  return experiment.item_count * experiment.repetitions;
}

/**
 * Why the selected experiments cannot be compared, or undefined when they can: 2 to 4 of one
 * dataset (the same `dataset_id`), or of one deleted dataset (no `dataset_id`, the same name),
 * as the server accepts them (D-J-13).
 */
export function compareBlockReason(
  experiments: readonly Schema<"Experiment">[],
): string | undefined {
  if (experiments.length < 2 || experiments.length > MAX_COMPARE_IDS) {
    return "Pick 2 to 4 experiments";
  }
  const [first] = experiments;
  const sameDataset = experiments.every((experiment) =>
    first?.dataset_id !== undefined
      ? experiment.dataset_id === first.dataset_id
      : experiment.dataset_id === undefined && experiment.dataset_name === first?.dataset_name,
  );
  return sameDataset ? undefined : "Pick experiments of the same dataset";
}
