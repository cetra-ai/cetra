import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { mulberry32 } from "@/mocks/generated/random";

/**
 * Experiments for `?mock=large`: 12 runs on the fixture dataset `faq` (the last one still running)
 * and 4 runs on a deleted dataset. Each run has the same 30 items per dataset, and its own
 * deterministic outputs. The handlers compute summaries and comparisons the way the server does
 * (`crates/server/src/experiments/mod.rs`). An id this mock does not know falls through to the
 * fixture handlers.
 */

const PROJECT = "/api/v1/projects/:project";
const DAY_MS = 86_400_000;
const FIRST_RUN_MS = Date.parse("2026-11-02T09:00:00.000Z");
const ITEM_COUNT = 30;
const RUNNING_RESULTS = 18;
const DEFAULT_THRESHOLD = 0.1;
const DEFAULT_LIMIT = 50;
const MAX_LIMIT = 200;
const FAQ_ID = "01900000-0000-7000-8000-000000000c01";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const MODELS = ["gpt-4.1", "claude-sonnet-4-5", "gemini-2.5-flash"] as const;
const PROMPTS = ["v1", "v2", "v3"] as const;
const TONES = ["neutral", "friendly", "formal", "curt"] as const;
const TOPICS = [
  "reset my password",
  "change my plan",
  "cancel a subscription",
  "get a refund",
  "update billing details",
  "export my data",
  "invite a teammate",
  "rotate an API key",
  "enable two-factor login",
  "download an invoice",
] as const;
const ANSWERS = [
  "Open Settings, choose Security, then select Reset password.",
  "Go to Billing, pick a plan, and confirm the change.",
  "Open Billing and choose Cancel subscription. Access lasts until the period ends.",
  "Refunds are issued within 5 days from the Billing page.",
  "Edit the company details under Billing, then save.",
  "Use Export under Account to get a JSON archive by email.",
  "Add the email under Team, then send the invitation.",
  "Create a new key under Developers and delete the old one.",
  "Turn on two-factor login under Security with an authenticator app.",
  "Find the invoice under Billing history and choose Download.",
] as const;

/** The item at `index` of `list`, wrapped around: the lists below are never empty. */
function pick<T>(list: readonly [T, ...T[]], index: number): T {
  return list[index % list.length] ?? list[0];
}

type ScoreDataType = Schema<"ScoreDataType">;
type Experiment = Schema<"Experiment">;
type ResultRow = Schema<"ResultRow">;
type ExperimentSummary = Schema<"ExperimentSummary">;
type ScoreSummary = Schema<"ScoreSummary">;
type ScoreDelta = Schema<"ScoreDelta">;
type CompareRow = Schema<"CompareRow">;
type CompareCell = Schema<"CompareCell">;
type CompareOnly = Schema<"CompareOnly">;

/** A dataset the runs point at. A deleted one has no id, as the server reports it. */
type DatasetRef = { id: string | undefined; name: string; version: number; offset: number };

const FAQ: DatasetRef = { id: FAQ_ID, name: "faq", version: 2, offset: 0 };
const LEGACY: DatasetRef = { id: undefined, name: "support-legacy", version: 1, offset: 1 };

type RunSpec = {
  dataset: DatasetRef;
  name: string;
  createdAt: number;
  model: string;
  prompt: string;
  /** The quality a run's rows centre on, from 0 to 1. */
  quality: number;
  /** The chance an answer is exactly right. */
  pass: number;
  running: boolean;
};

type Run = {
  experiment: Experiment;
  /** The rows the run holds so far, by item id, one repetition each. */
  rows: ResultRow[];
  /** The dataset key: the id, or the name of a deleted dataset. */
  datasetKey: string;
};

const SPECS: RunSpec[] = [
  ...Array.from(
    { length: 12 },
    (_, n): RunSpec => ({
      dataset: FAQ,
      name: `nightly-${String(n + 1).padStart(2, "0")}`,
      createdAt: FIRST_RUN_MS + n * DAY_MS,
      model: pick(MODELS, n),
      prompt: pick(PROMPTS, Math.floor(n / 4)),
      quality: 0.5 + 0.02 * n,
      pass: Math.min(0.9, 0.45 + 0.03 * n),
      running: n === 11,
    }),
  ),
  ...Array.from(
    { length: 4 },
    (_, k): RunSpec => ({
      dataset: LEGACY,
      name: `legacy-${k + 1}`,
      createdAt: FIRST_RUN_MS - 30 * DAY_MS + k * DAY_MS,
      model: pick(MODELS, k),
      prompt: "v1",
      quality: 0.45 + 0.01 * k,
      pass: 0.4,
      running: false,
    }),
  ),
];

/** Runs newest first, as the list returns them. */
const RUNS: Run[] = SPECS.map((spec, ordinal) => buildRun(spec, ordinal)).sort((left, right) =>
  right.experiment.created_at.localeCompare(left.experiment.created_at),
);
const RUN_BY_ID = new Map(RUNS.map((run) => [run.experiment.id, run]));

function uuidOf(n: number): string {
  return `01900000-0000-7000-8000-${n.toString(16).padStart(12, "0")}`;
}

function itemId(datasetOffset: number, index: number): string {
  return uuidOf(0xb0000 + datasetOffset * 0x100 + index);
}

function buildRun(spec: RunSpec, ordinal: number): Run {
  const count = spec.running ? RUNNING_RESULTS : ITEM_COUNT;
  const rows = buildRows(spec, ordinal, count);
  const created = new Date(spec.createdAt).toISOString();
  const experiment: Experiment = {
    id: uuidOf(0xe0000 + ordinal),
    name: spec.name,
    dataset_name: spec.dataset.name,
    dataset_version: spec.dataset.version,
    // The schema types a metadata value as `never`; the mock stores the strings of a run.
    metadata: { model: spec.model, prompt: spec.prompt } as unknown as Experiment["metadata"],
    repetitions: 1,
    item_count: ITEM_COUNT,
    result_count: rows.length,
    status: spec.running ? "running" : "completed",
    created_at: created,
    ...(spec.dataset.id ? { dataset_id: spec.dataset.id } : {}),
    ...(spec.running ? {} : { finished_at: new Date(spec.createdAt + 5 * 60_000).toISOString() }),
  };
  return {
    experiment,
    rows,
    datasetKey: spec.dataset.id ?? `deleted:${spec.dataset.name}`,
  };
}

function buildRows(spec: RunSpec, ordinal: number, count: number): ResultRow[] {
  const rng = mulberry32(ordinal * 7919 + 17);
  const rows: ResultRow[] = [];
  for (let index = 0; index < count; index += 1) {
    // Six draws per row, in this order, whatever the row's outcome, so one row never shifts another.
    const failed = rng() < 0.03;
    const exact = rng() < spec.pass;
    const quality = round(clamp(spec.quality + (rng() - 0.5) * 0.3), 2);
    const tone = pick(TONES, Math.floor(rng() * TONES.length));
    const latencyNs = Math.round((1.5 + rng() * 4.5) * 1e9);
    const cost = round(0.002 + rng() * 0.01, 4);
    const expected = pick(ANSWERS, index);
    const row: ResultRow = {
      dataset_item_id: itemId(spec.dataset.offset, index),
      repetition: 0,
      input: { question: `Question ${index + 1}: how do I ${pick(TOPICS, index)}?` },
      expected_output: expected,
      latency_ns: latencyNs,
      cost_total: cost,
      scores: {},
    };
    if (failed) {
      row.error = "The model call timed out after 30 seconds.";
    } else {
      row.output = exact
        ? `${expected}\n\n- The steps are in the help center.\n- Tone: ${tone}.`
        : "I am not sure about that. Please contact support, who can check your account.";
      row.scores = {
        exact: { data_type: "boolean", value_num: exact ? 1 : 0 },
        quality: { data_type: "numeric", value_num: quality },
        tone: { data_type: "categorical", value_str: tone },
      };
    }
    rows.push(row);
  }
  return rows;
}

function clamp(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function round(value: number, digits: number): number {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

// Summaries, as the server computes them (rule 4 and rule 5).

function summaryOf(run: Run): ExperimentSummary {
  const types = new Map<string, ScoreDataType>();
  for (const row of run.rows) {
    for (const [name, score] of Object.entries(row.scores)) {
      if (!types.has(name)) {
        types.set(name, score.data_type);
      }
    }
  }
  const scores = [...types].map(([name, dataType]) => scoreSummaryOf(run.rows, name, dataType));
  const latencies = run.rows
    .flatMap((row) => (row.latency_ns === undefined ? [] : [row.latency_ns]))
    .sort((left, right) => left - right);
  const costs = run.rows.flatMap((row) => (row.cost_total === undefined ? [] : [row.cost_total]));
  const errors = run.rows.filter((row) => row.error !== undefined).length;
  return {
    experiment: run.experiment,
    scores,
    result_count: run.rows.length,
    error_count: errors,
    ...(latencies.length > 0
      ? {
          latency_mean_ns: Math.trunc(
            latencies.reduce((sum, ns) => sum + ns, 0) / latencies.length,
          ),
          latency_p50_ns: nearestRank(latencies, 50),
          latency_p95_ns: nearestRank(latencies, 95),
        }
      : {}),
    ...(costs.length > 0 ? { cost_total: costs.reduce((sum, cost) => sum + cost, 0) } : {}),
  };
}

function scoreSummaryOf(rows: ResultRow[], name: string, dataType: ScoreDataType): ScoreSummary {
  const values: number[] = [];
  const labels: Record<string, number> = {};
  let count = 0;
  for (const row of rows) {
    const score = row.scores[name];
    if (!score) {
      continue;
    }
    count += 1;
    if (score.value_num !== undefined) {
      values.push(score.value_num);
    }
    if (dataType === "categorical" && score.value_str !== undefined) {
      labels[score.value_str] = (labels[score.value_str] ?? 0) + 1;
    }
  }
  values.sort((left, right) => left - right);
  const numeric = dataType !== "text";
  const mean =
    values.length > 0 ? values.reduce((sum, v) => sum + v, 0) / values.length : undefined;
  const first = values[0];
  const last = values[values.length - 1];
  return {
    name,
    data_type: dataType,
    count,
    labels,
    histogram: numeric ? histogramOf(dataType, values) : [],
    ...(numeric && mean !== undefined ? { mean } : {}),
    ...(numeric && values.length > 0 ? { median: medianOf(values), min: first, max: last } : {}),
    ...(dataType === "boolean" && mean !== undefined ? { pass_rate: mean } : {}),
  };
}

function medianOf(sorted: number[]): number {
  const middle = Math.floor(sorted.length / 2);
  const upper = sorted[middle] ?? 0;
  return sorted.length % 2 === 1 ? upper : ((sorted[middle - 1] ?? 0) + upper) / 2;
}

function histogramOf(dataType: ScoreDataType, sorted: number[]): number[] {
  const min = sorted[0];
  const max = sorted[sorted.length - 1];
  if (dataType !== "numeric" || min === undefined || max === undefined) {
    return [];
  }
  if (max === min) {
    return [sorted.length];
  }
  const buckets = new Array<number>(10).fill(0);
  for (const value of sorted) {
    const index = Math.floor(((value - min) / (max - min)) * 10);
    const slot = Math.min(9, Math.max(0, index));
    buckets[slot] = (buckets[slot] ?? 0) + 1;
  }
  return buckets;
}

function nearestRank(sorted: number[], percent: number): number | undefined {
  const rank = Math.ceil((percent * sorted.length) / 100);
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank - 1))];
}

// Comparison, as the server computes it (rules 8 to 12).

type Cell = { scores: Map<string, number>; hasError: boolean; row: ResultRow };

function cellsOf(rows: ResultRow[]): Map<string, Cell> {
  const cells = new Map<string, Cell>();
  for (const row of rows) {
    const scores = new Map<string, number>();
    for (const [name, score] of Object.entries(row.scores)) {
      if (score.value_num !== undefined) {
        scores.set(name, score.value_num);
      }
    }
    cells.set(row.dataset_item_id, { scores, hasError: row.error !== undefined, row });
  }
  return cells;
}

function regressed(candidate: Cell, baseline: Cell, threshold: number): boolean {
  const lower = [...candidate.scores].some(([name, value]) => {
    const base = baseline.scores.get(name);
    return base !== undefined && value < base - threshold;
  });
  return lower || (!baseline.hasError && candidate.hasError);
}

function improved(candidate: Cell, baseline: Cell, threshold: number): boolean {
  const higher = [...candidate.scores].some(([name, value]) => {
    const base = baseline.scores.get(name);
    return base !== undefined && value > base + threshold;
  });
  return higher || (baseline.hasError && !candidate.hasError);
}

function itemMatches(
  baseline: Map<string, Cell>,
  candidates: Map<string, Cell>[],
  item: string,
  threshold: number,
  only: CompareOnly,
): boolean {
  const base = baseline.get(item);
  if (!base) {
    return false;
  }
  return candidates.some((cells) => {
    const cell = cells.get(item);
    if (!cell) {
      return false;
    }
    if (only === "regressed") {
      return regressed(cell, base, threshold);
    }
    if (only === "improved") {
      return improved(cell, base, threshold);
    }
    return regressed(cell, base, threshold) || improved(cell, base, threshold);
  });
}

function scoreDelta(
  baseline: Map<string, Cell>,
  candidate: Map<string, Cell>,
  name: string,
  threshold: number,
): ScoreDelta | undefined {
  const baseValues: number[] = [];
  const candidateValues: number[] = [];
  let improvedCount = 0;
  let regressedCount = 0;
  for (const [item, cell] of candidate) {
    const base = baseline.get(item);
    if (!base || base.hasError || cell.hasError) {
      continue;
    }
    const baseValue = base.scores.get(name);
    const value = cell.scores.get(name);
    if (baseValue === undefined || value === undefined) {
      continue;
    }
    baseValues.push(baseValue);
    candidateValues.push(value);
    if (value - baseValue > threshold) {
      improvedCount += 1;
    } else if (value - baseValue < -threshold) {
      regressedCount += 1;
    }
  }
  if (baseValues.length === 0) {
    return undefined;
  }
  return {
    delta: mean(candidateValues) - mean(baseValues),
    n_compared: baseValues.length,
    improved: improvedCount,
    regressed: regressedCount,
  };
}

function mean(values: number[]): number {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

function compareRow(
  item: string,
  loaded: Loaded[],
  baseline: Map<string, Cell>,
  threshold: number,
): CompareRow | undefined {
  const cells: CompareCell[] = [];
  let source: ResultRow | undefined;
  for (const [index, entry] of loaded.entries()) {
    const cell = entry.cells.get(item);
    if (!cell) {
      continue;
    }
    source ??= cell.row;
    const base = baseline.get(item);
    cells.push({
      experiment_id: entry.run.experiment.id,
      scores: Object.fromEntries(cell.scores),
      regressed: index > 0 && base !== undefined && regressed(cell, base, threshold),
      improved: index > 0 && base !== undefined && improved(cell, base, threshold),
      ...(cell.row.output !== undefined ? { output: cell.row.output } : {}),
      ...(cell.row.error !== undefined ? { error: cell.row.error } : {}),
      ...(cell.row.latency_ns !== undefined ? { latency_ns: cell.row.latency_ns } : {}),
    });
  }
  if (!source) {
    return undefined;
  }
  return {
    dataset_item_id: item,
    input: source.input,
    ...(source.expected_output !== undefined ? { expected_output: source.expected_output } : {}),
    cells,
  };
}

type Loaded = { run: Run; cells: Map<string, Cell> };

function compareBody(
  runs: Run[],
  threshold: number,
  only: CompareOnly | undefined,
  limit: number,
  after: string | undefined,
): Record<string, unknown> {
  const loaded: Loaded[] = runs.map((run) => ({ run, cells: cellsOf(run.rows) }));
  const [first, ...rest] = loaded;
  const baseline = first?.cells ?? new Map<string, Cell>();
  const candidates = rest.map((entry) => entry.cells);
  const items = [...new Set(loaded.flatMap((entry) => [...entry.cells.keys()]))].sort();
  const matching = items
    .filter((item) => after === undefined || item > after)
    .filter(
      (item) => only === undefined || itemMatches(baseline, candidates, item, threshold, only),
    );
  const page = matching.slice(0, limit);
  const next = matching.length > limit ? (page[page.length - 1] ?? null) : null;
  const rows = page.flatMap((item) => {
    const row = compareRow(item, loaded, baseline, threshold);
    return row ? [row] : [];
  });

  const names = new Set<string>();
  for (const entry of loaded) {
    for (const cell of entry.cells.values()) {
      for (const name of cell.scores.keys()) {
        names.add(name);
      }
    }
  }
  const deltas = rest.map((entry) => {
    const scores: Record<string, ScoreDelta> = {};
    for (const name of names) {
      const delta = scoreDelta(baseline, entry.cells, name, threshold);
      if (delta) {
        scores[name] = delta;
      }
    }
    let improvedItems = 0;
    let regressedItems = 0;
    for (const [item, cell] of entry.cells) {
      const base = baseline.get(item);
      if (!base) {
        continue;
      }
      improvedItems += improved(cell, base, threshold) ? 1 : 0;
      regressedItems += regressed(cell, base, threshold) ? 1 : 0;
    }
    return {
      experiment_id: entry.run.experiment.id,
      scores,
      improved_items: improvedItems,
      regressed_items: regressedItems,
    };
  });

  return {
    experiments: runs.map(summaryOf),
    deltas,
    warnings: warningsOf(loaded, baseline),
    rows,
    next_cursor: next,
  };
}

function warningsOf(loaded: Loaded[], baseline: Map<string, Cell>): string[] {
  const warnings: string[] = [];
  const versions = new Set(loaded.map((entry) => entry.run.experiment.dataset_version));
  if (versions.size > 1) {
    const named = loaded.map(
      (entry) => `${entry.run.experiment.id} at version ${entry.run.experiment.dataset_version}`,
    );
    warnings.push(`experiments use different dataset versions: ${named.join(", ")}`);
  }
  const candidates = loaded.slice(1);
  const missing = new Set<string>();
  for (const entry of candidates) {
    for (const item of entry.cells.keys()) {
      if (!baseline.has(item)) {
        missing.add(item);
      }
    }
  }
  if (missing.size > 0) {
    warnings.push(`${missing.size} items have no result in the baseline`);
  }
  for (const entry of candidates) {
    const union = new Set([...baseline.keys(), ...entry.cells.keys()]);
    let bothRan = 0;
    for (const [item, cell] of entry.cells) {
      const base = baseline.get(item);
      if (!cell.hasError && base && !base.hasError) {
        bothRan += 1;
      }
    }
    const leftOut = union.size - bothRan;
    if (leftOut > 0) {
      warnings.push(
        `${entry.run.experiment.id}: ${leftOut} items are left out of the deltas (missing or failed in one of the two)`,
      );
    }
  }
  return warnings;
}

// Handlers.

/** The experiment handlers of `?mock=large`. They come before the fixture handlers. */
export function experimentHandlers(): HttpHandler[] {
  return [
    http.get(`${PROJECT}/experiments`, ({ request }) => listExperiments(new URL(request.url))),
    http.get(`${PROJECT}/experiments/compare`, ({ request }) => compareRoute(new URL(request.url))),
    http.get(`${PROJECT}/experiments/summaries`, ({ request }) =>
      summariesRoute(new URL(request.url)),
    ),
    http.get(`${PROJECT}/experiments/:id/results`, ({ request, params }) =>
      resultsRoute(new URL(request.url), text(params.id)),
    ),
    http.get(`${PROJECT}/experiments/:id`, ({ params }) => oneExperiment(text(params.id))),
  ];
}

function listExperiments(url: URL): Response {
  const page = readPage(url);
  if (page instanceof Response) {
    return page;
  }
  const dataset = url.searchParams.get("dataset");
  const rows = RUNS.filter(
    (run) =>
      dataset === null ||
      run.experiment.dataset_id === dataset ||
      run.experiment.dataset_name === dataset,
  );
  const offset = decodeOffset(url.searchParams.get("cursor"));
  if (offset instanceof Response) {
    return offset;
  }
  const next = offset + page.limit < rows.length ? btoa(String(offset + page.limit)) : null;
  const data = rows.slice(offset, offset + page.limit).map((run) => run.experiment);
  return HttpResponse.json({ data, next_cursor: next });
}

function oneExperiment(id: string): Response | undefined {
  const run = RUN_BY_ID.get(id);
  return run ? HttpResponse.json(run.experiment) : undefined;
}

function summariesRoute(url: URL): Response | undefined {
  const ids = idsOf(url);
  if (ids.length > 0 && !ids.some((id) => RUN_BY_ID.has(id))) {
    return undefined;
  }
  const invalid = idsProblem(ids, 1, 25);
  if (invalid) {
    return invalid;
  }
  const data = ids.flatMap((id) => {
    const run = RUN_BY_ID.get(id);
    return run ? [summaryOf(run)] : [];
  });
  return HttpResponse.json({ data, skipped: [] });
}

function compareRoute(url: URL): Response | undefined {
  const ids = idsOf(url);
  if (ids.length > 0 && !ids.some((id) => RUN_BY_ID.has(id))) {
    return undefined;
  }
  const invalid = idsProblem(ids, 2, 4);
  if (invalid) {
    return invalid;
  }
  const runs: Run[] = [];
  for (const id of ids) {
    const run = RUN_BY_ID.get(id);
    if (!run) {
      return problem(404, "not_found", "Not Found");
    }
    runs.push(run);
  }
  const datasets = new Set(runs.map((run) => run.datasetKey));
  if (datasets.size > 1) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: "the experiments must belong to one dataset" },
    ]);
  }

  const thresholdText = url.searchParams.get("threshold");
  const threshold = thresholdText === null ? DEFAULT_THRESHOLD : Number(thresholdText);
  if (!Number.isFinite(threshold) || threshold < 0 || threshold > 1000) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "threshold", message: "must be between 0 and 1000" },
    ]);
  }

  const onlyText = url.searchParams.get("only");
  if (onlyText !== null && !["regressed", "improved", "changed"].includes(onlyText)) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "only", message: "must be regressed, improved or changed" },
    ]);
  }
  const page = readPage(url);
  if (page instanceof Response) {
    return page;
  }
  const cursor = url.searchParams.get("cursor");
  if (cursor !== null && !UUID.test(cursor)) {
    return problem(400, "bad_request", "Bad Request");
  }

  return HttpResponse.json(
    compareBody(
      runs,
      threshold,
      (onlyText ?? undefined) as CompareOnly | undefined,
      page.limit,
      cursor ?? undefined,
    ),
  );
}

function resultsRoute(url: URL, id: string): Response | undefined {
  const run = RUN_BY_ID.get(id);
  if (!run) {
    return undefined;
  }
  const page = readPage(url);
  if (page instanceof Response) {
    return page;
  }
  const errorsOnly = flag(url.searchParams.get("errors_only"));
  const cursor = url.searchParams.get("cursor");
  let after: { item: string; repetition: number } | undefined;
  if (cursor !== null) {
    const match = /^([0-9a-f-]{36}):(\d+)$/i.exec(cursor);
    if (!match?.[1] || !UUID.test(match[1])) {
      return problem(400, "bad_request", "Bad Request");
    }
    after = { item: match[1], repetition: Number(match[2]) };
  }
  const rows = run.rows
    .filter((row) => !errorsOnly || row.error !== undefined)
    .filter(
      (row) =>
        after === undefined ||
        row.dataset_item_id > after.item ||
        (row.dataset_item_id === after.item && row.repetition > after.repetition),
    );
  const shown = rows.slice(0, page.limit);
  const last = shown[shown.length - 1];
  const next =
    rows.length > page.limit && last ? `${last.dataset_item_id}:${last.repetition}` : null;
  return HttpResponse.json({ summary: summaryOf(run), rows: shown, next_cursor: next });
}

function idsOf(url: URL): string[] {
  const raw = url.searchParams.get("ids");
  return raw === null || raw === "" ? [] : raw.split(",");
}

function idsProblem(ids: string[], min: number, max: number): Response | undefined {
  const distinct = new Set(ids);
  if (ids.length < min || ids.length > max) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: `between ${min} and ${max} experiment ids` },
    ]);
  }
  if (!ids.every((id) => UUID.test(id)) || distinct.size !== ids.length) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: "distinct experiment ids" },
    ]);
  }
  return undefined;
}

function readPage(url: URL): { limit: number } | Response {
  const text = url.searchParams.get("limit");
  if (text === null) {
    return { limit: DEFAULT_LIMIT };
  }
  const limit = Number(text);
  if (!Number.isInteger(limit) || limit < 1 || limit > MAX_LIMIT) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "limit", message: `between 1 and ${MAX_LIMIT}` },
    ]);
  }
  return { limit };
}

function decodeOffset(value: string | null): number | Response {
  if (value === null) {
    return 0;
  }
  try {
    const decoded = atob(value);
    if (/^\d+$/.test(decoded)) {
      return Number(decoded);
    }
  } catch {
    // Fall through to the 400 below.
  }
  return problem(400, "bad_request", "Bad Request");
}

function flag(value: string | null): boolean {
  return value === "true" || value === "1";
}

function text(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

function problem(
  status: number,
  code: Schema<"ProblemCode">,
  title: string,
  errors?: Schema<"FieldError">[],
): Response {
  return HttpResponse.json(
    {
      type: "about:blank",
      title,
      status,
      code,
      ...(errors ? { errors } : {}),
    },
    { status, headers: { "Content-Type": "application/problem+json" } },
  );
}
