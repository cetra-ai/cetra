import type { Schema } from "@/api/types";
import { parseFlag } from "@/lib/search/common";

export type CompareOnly = Schema<"CompareOnly">;

export const DEFAULT_THRESHOLD = 0.1;
/** A running experiment younger than this is polled for results (CG-J5). */
const LIVE_WINDOW_MS = 24 * 60 * 60 * 1000;
/** How often a live experiment and its results are refetched. */
export const LIVE_POLL_MS = 5000;

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

export type ExperimentStatus = Schema<"Experiment">["status"];
const STATUSES: readonly ExperimentStatus[] = ["running", "completed", "failed"];

/**
 * `?dataset=`, `?status=` and `?experiment=` of the list: a UUID string for the first and last,
 * one of the statuses for `status`; anything else is dropped.
 */
export function parseExperimentsSearch(search: Record<string, unknown>): {
  dataset?: string;
  status?: ExperimentStatus;
  experiment?: string;
} {
  const dataset = stringParam(search, "dataset");
  const experiment = stringParam(search, "experiment");
  const status = STATUSES.find((candidate) => candidate === stringParam(search, "status"));
  return {
    ...(dataset && UUID.test(dataset) ? { dataset } : {}),
    ...(status === undefined ? {} : { status }),
    ...(experiment && UUID.test(experiment) ? { experiment } : {}),
  };
}

/** The ids of a compare search (`parseCompareSearch().ids`) as a list, in URL order. */
export function compareIdList(ids: string): string[] {
  return ids === "" ? [] : ids.split(",");
}

/**
 * `?errors=1` and `?item=<uuid>:<repetition>` of the experiment page. `item` is kept only when it
 * is a UUID, a colon and a non-negative integer (`parseItemKey`).
 */
export function parseExperimentSearch(search: Record<string, unknown>): {
  errors?: 1;
  item?: string;
} {
  const item = stringParam(search, "item");
  return {
    ...(parseFlag(search.errors) ? { errors: 1 as const } : {}),
    ...(item !== undefined && parseItemKey(item) !== undefined ? { item } : {}),
  };
}

/** The key of one result row in the URL: `<dataset_item_id>:<repetition>`. */
export function itemKey(row: { dataset_item_id: string; repetition: number }): string {
  return `${row.dataset_item_id}:${row.repetition}`;
}

/** Splits an item key back into its item id and repetition; undefined when malformed. */
export function parseItemKey(
  key: string,
): { datasetItemId: string; repetition: number } | undefined {
  const match = /^([0-9a-f-]+):(0|[1-9][0-9]*)$/i.exec(key);
  if (match === null || match[1] === undefined || match[2] === undefined) {
    return undefined;
  }
  const datasetItemId = match[1];
  if (!UUID.test(datasetItemId)) {
    return undefined;
  }
  return { datasetItemId, repetition: Number(match[2]) };
}

/**
 * Whether an experiment is still running and younger than a day, so its page polls
 * (`LIVE_POLL_MS`). `now` is `Date.now()`.
 */
export function isLiveExperiment(
  experiment: Schema<"Experiment"> | undefined,
  now: number,
): boolean {
  if (experiment?.status !== "running") {
    return false;
  }
  const started = Date.parse(experiment.created_at);
  return !Number.isNaN(started) && now - started < LIVE_WINDOW_MS;
}

/** A threshold from the URL: a finite number, or a numeric string, in [0, 1000]. */
function thresholdOf(value: unknown): number | undefined {
  let number = Number.NaN;
  if (typeof value === "number") {
    number = value;
  } else if (typeof value === "string" && value.trim() !== "") {
    number = Number(value);
  }
  return Number.isFinite(number) && number >= 0 && number <= MAX_THRESHOLD ? number : undefined;
}

const COMPARE_ONLY: readonly CompareOnly[] = ["regressed", "improved", "changed"];

/**
 * `?ids=a,b&threshold=0.2&only=regressed&item=<uuid>` of the compare view. `ids`: split on commas,
 * trimmed, empty parts dropped, duplicates dropped (first kept), UUID strings only, at most 4 kept,
 * and joined back with commas (the URL keeps the string; `compareIdList` splits it). `threshold`: a
 * finite number in [0, 1000], else undefined (the view uses DEFAULT_THRESHOLD). `only`: one of
 * `regressed`, `improved`, `changed`, else undefined (every item). `item`: a UUID, else undefined.
 */
export function parseCompareSearch(search: Record<string, unknown>): {
  ids: string;
  threshold?: number;
  only?: CompareOnly;
  item?: string;
} {
  const ids: string[] = [];
  for (const part of (stringParam(search, "ids") ?? "").split(",")) {
    const id = part.trim();
    if (id && UUID.test(id) && !ids.includes(id) && ids.length < MAX_COMPARE_IDS) {
      ids.push(id);
    }
  }
  const threshold = thresholdOf(search.threshold);
  const only = COMPARE_ONLY.find((candidate) => candidate === stringParam(search, "only"));
  const item = stringParam(search, "item");
  return {
    ids: ids.join(","),
    ...(threshold !== undefined ? { threshold } : {}),
    ...(only === undefined ? {} : { only }),
    ...(item !== undefined && UUID.test(item) ? { item } : {}),
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

type DataType = Schema<"ScoreSummary">["data_type"];

/** The magnitude as `deltaText` prints it: points for a boolean score, three decimals otherwise. */
function printedMagnitude(delta: number, dataType: DataType): { value: number; digits: number } {
  const scale = dataType === "boolean" ? 100 : 1;
  const digits = dataType === "boolean" ? 1 : 3;
  return { value: Number((Math.abs(delta) * scale).toFixed(digits)), digits };
}

/**
 * A delta as "+0.120" / "−0.050" (U+2212) / "0.000", three decimals; a boolean score's delta
 * in points: "+12.5 pts".
 */
export function deltaText(delta: number, dataType: DataType): string {
  const { value, digits } = printedMagnitude(delta, dataType);
  const sign = value === 0 ? "" : delta > 0 ? "+" : MINUS;
  const number = value.toFixed(digits);
  return dataType === "boolean" ? `${sign}${number} pts` : `${sign}${number}`;
}

/**
 * "better" when the printed delta is above 0, "worse" when below, "same" when it prints as zero
 * (CG-J1: higher is better). It reads the rounded value, so −0.0004 is "same".
 */
export function deltaDirection(delta: number, dataType: DataType): "better" | "worse" | "same" {
  if (printedMagnitude(delta, dataType).value === 0) {
    return "same";
  }
  return delta > 0 ? "better" : "worse";
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
    case "text":
      return value.value_str === undefined ? "—" : truncateChars(value.value_str);
  }
}

/** A text cut to `MAX_TEXT_CHARS` characters, with "…" when it was cut. */
function truncateChars(text: string): string {
  const chars = [...text];
  return chars.length > MAX_TEXT_CHARS ? `${chars.slice(0, MAX_TEXT_CHARS).join("")}…` : text;
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

/** The colour class of a delta's direction (higher is better, CG-J1). */
export const DIRECTION_CLASS = {
  better: "text-status-ok",
  worse: "text-status-error",
  same: "text-muted-foreground",
} as const;

/** What groups experiments into one dataset's runs: `dataset_id`, or the deleted dataset's name. */
export function datasetKey(experiment: Schema<"Experiment">): string {
  return experiment.dataset_id ?? `deleted:${experiment.dataset_name}`;
}

/**
 * Each experiment's previous run: the next older experiment of the same dataset in `rows`, which
 * come newest first (`ORDER BY e.id DESC`). An experiment with no older run is not in the map.
 */
export function previousRuns(rows: readonly Schema<"Experiment">[]): Map<string, string> {
  const previous = new Map<string, string>();
  const lastVisited = new Map<string, string>();
  // From the oldest row to the newest: each row finds the older run of its dataset visited last.
  for (let index = rows.length - 1; index >= 0; index--) {
    const row = rows[index];
    if (row === undefined) {
      continue;
    }
    const key = datasetKey(row);
    const older = lastVisited.get(key);
    if (older !== undefined) {
      previous.set(row.id, older);
    }
    lastVisited.set(key, row.id);
  }
  return previous;
}

/** The value a score summary is headlined by: the mean (numeric), the pass rate (boolean). */
export function headlineValue(summary: Schema<"ScoreSummary">): number | undefined {
  switch (summary.data_type) {
    case "numeric":
      return summary.mean;
    case "boolean":
      return summary.pass_rate;
    case "categorical":
    case "text":
      return undefined;
  }
}

/**
 * The headline as text: the mean with 3 decimals, the pass rate in %, a categorical score's most
 * frequent label with its share of the labelled rows ("friendly 40 %"), a text score's row
 * count ("n 12"); "—" when there is nothing to show.
 */
export function headlineText(summary: Schema<"ScoreSummary">): string {
  if (summary.data_type === "categorical") {
    const labels = Object.entries(summary.labels);
    const total = labels.reduce((sum, [, count]) => sum + count, 0);
    const [top] = labels.sort(
      ([nameA, countA], [nameB, countB]) => countB - countA || nameA.localeCompare(nameB),
    );
    return top === undefined || total === 0 ? "—" : `${top[0]} ${percentText(top[1] / total)} %`;
  }
  if (summary.data_type === "text") {
    return summary.count > 0 ? `n ${summary.count}` : "—";
  }
  const value = headlineValue(summary);
  if (value === undefined) {
    return "—";
  }
  return summary.data_type === "boolean" ? `${percentText(value)} %` : value.toFixed(3);
}

/**
 * A dataset item id shortened for a table: its last 8 characters. Item ids are UUIDv7, and items
 * of one dataset are often created in the same millisecond, so their first characters match.
 */
export function itemShortId(id: string): string {
  return id.slice(-8);
}

/**
 * The metadata chips of an experiment: `[key, text]` for each string, number and boolean value,
 * in the order the object holds them. A string is cut like a score text (40 characters, "…").
 */
export function metadataTags(metadata: Record<string, unknown>): [string, string][] {
  const tags: [string, string][] = [];
  for (const [key, value] of Object.entries(metadata)) {
    if (typeof value === "string") {
      tags.push([key, truncateChars(value)]);
    } else if (typeof value === "number" || typeof value === "boolean") {
      tags.push([key, String(value)]);
    }
  }
  return tags;
}

/** The most ids one `GET …/experiments/summaries` takes (1 to 25). */
export const MAX_SUMMARY_IDS = 25;

/** `rows` split into runs of at most `MAX_SUMMARY_IDS`, in order. */
export function summaryChunks<T>(rows: readonly T[]): T[][] {
  const chunks: T[][] = [];
  for (let start = 0; start < rows.length; start += MAX_SUMMARY_IDS) {
    chunks.push(rows.slice(start, start + MAX_SUMMARY_IDS));
  }
  return chunks;
}
