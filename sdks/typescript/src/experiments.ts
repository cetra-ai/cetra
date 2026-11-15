/**
 * Experiments: `evaluate` runs a task over every item of a dataset version,
 * traces each run, scores it on this machine and stores the results on the
 * server (spec 06 § Protocol as amended; D-J-10, D-J-12, D-J-16).
 *
 * Task and scorer exceptions are recorded per run and do not abort the
 * experiment. Configuration, argument and API failures raise.
 */

import { context, TraceFlags, trace } from "@opentelemetry/api";

import { ApiError, Client, segment } from "./client.js";
import { ConfigError } from "./config.js";
import { experimentContext } from "./context.js";
import {
  Dataset,
  type DatasetItem,
  itemFromWire,
  type WireItem,
  type WirePage,
} from "./datasets.js";
import { debug, warn, warnOnce } from "./log.js";
import { currentRuntime, type Runtime } from "./runtime.js";
import { queueExperimentScore, type ScoreDataType, valueReason } from "./scores.js";
import { toJsonable } from "./serialize.js";
import { type SpanHandle, withSpan } from "./span.js";
import { runtimeGlobals, utf8Decode, utf8Encode } from "./web.js";

const DEFAULT_CONCURRENCY = 8;
const MAX_CONCURRENCY = 64;
const MAX_REPETITIONS = 20;
const MAX_NAME_CHARS = 200;
const ITEM_PAGE_SIZE = 1_000;
const UPLOAD_BATCH_ROWS = 500;
const UPLOAD_BATCH_BYTES = 32 * 1024 * 1024;
/** An output over this many UTF-8 bytes of JSON text is cut (§ Output). */
const OUTPUT_LIMIT_BYTES = 262_144;
const OUTPUT_KEEP_BYTES = 260_000;
const ERROR_LIMIT_CHARS = 4_096;
const UPLOAD_FAILED = "results upload failed";

/** A scorer's rich return value (spec 10). */
export interface Score {
  value: boolean | number | string;
  reason?: string;
  dataType?: ScoreDataType;
  metadata?: Readonly<Record<string, unknown>>;
}

/** What a scorer receives for one run. */
export interface ScorerArgs {
  input: unknown;
  output: unknown;
  expected: unknown;
  metadata: Readonly<Record<string, unknown>>;
  traceId: string | undefined;
  item: DatasetItem;
}

/** A scorer's return value: a bare value, a `Score`, or nothing (no score). */
export type ScorerResult = boolean | number | string | Score | undefined | null;

export type Scorer = (args: ScorerArgs) => ScorerResult | Promise<ScorerResult>;

export interface EvaluateOptions {
  dataset: string | Dataset;
  task: (input: unknown, item: DatasetItem) => unknown;
  scorers?: readonly Scorer[];
  experimentName?: string;
  description?: string;
  metadata?: Readonly<Record<string, unknown>>;
  datasetVersion?: number;
  /** 1 to 64; default 8. */
  concurrency?: number;
  /** 1 to 20; default 1. */
  repetitions?: number;
  client?: Client;
}

/** One task run of an experiment. */
export interface Run {
  readonly datasetItemId: string;
  /** 0-based: 0 to `repetitions - 1`. */
  readonly repetition: number;
  readonly input: unknown;
  readonly expectedOutput: unknown;
  /** The task's value before conversion; `undefined` for a failed run. */
  readonly output: unknown;
  readonly error: string | undefined;
  readonly traceId: string | undefined;
  readonly latencyNs: number;
  readonly scores: Readonly<Record<string, Score>>;
}

/** The outcome of `evaluate`, built from the local runs and scores. */
export class ExperimentResult {
  readonly experimentId: string;
  readonly experimentName: string;
  readonly datasetId: string;
  /** The pinned dataset version. */
  readonly datasetVersion: number;
  /** In item order, repetitions inner. */
  readonly runs: readonly Run[];
  /** Scorer name to the number of runs it failed on (only scorers that failed). */
  readonly scorerErrors: Readonly<Record<string, number>>;
  readonly #scorerNames: readonly string[];

  /** @internal */
  constructor(fields: {
    experimentId: string;
    experimentName: string;
    datasetId: string;
    datasetVersion: number;
    runs: readonly Run[];
    scorerErrors: Readonly<Record<string, number>>;
    scorerNames: readonly string[];
  }) {
    this.experimentId = fields.experimentId;
    this.experimentName = fields.experimentName;
    this.datasetId = fields.datasetId;
    this.datasetVersion = fields.datasetVersion;
    this.runs = fields.runs;
    this.scorerErrors = fields.scorerErrors;
    this.#scorerNames = fields.scorerNames;
  }

  /** Runs with a task error. */
  get errorCount(): number {
    return this.runs.filter((run) => run.error !== undefined).length;
  }

  /** A short text report, one line per scorer, in the order the scorers were given. */
  summary(): string {
    const lines = [
      `Experiment ${this.experimentName} (${this.experimentId}) on dataset ${this.datasetId} v${String(this.datasetVersion)}: ${String(this.runs.length)} runs, ${String(this.errorCount)} errors`,
    ];
    for (const name of this.#scorerNames) {
      const values = this.#values(name);
      const numbers = values.flatMap((value) => (typeof value === "string" ? [] : [Number(value)]));
      const labels = values.filter((value): value is string => typeof value === "string");
      if (numbers.length === 0 && labels.length === 0) {
        lines.push(`  ${name}: no values`);
      }
      if (numbers.length > 0) {
        lines.push(
          `  ${name}: mean ${mean(numbers).toFixed(3)} (${String(numbers.length)} values)`,
        );
      }
      if (labels.length > 0) {
        lines.push(`  ${name}: ${labelCounts(labels)}`);
      }
      const errors = this.#errorsOf(name);
      if (errors > 0) {
        lines.push(`  ${name}: ${String(errors)} errors`);
      }
    }
    return lines.join("\n");
  }

  /**
   * Throws `Error` when the mean of the numeric and boolean values of `name`
   * (over every run) is below `threshold`, or when there are none.
   */
  assertMin(name: string, threshold: number): void {
    const numbers = this.#values(name).flatMap((value) =>
      typeof value === "string" ? [] : [Number(value)],
    );
    if (numbers.length === 0) {
      throw new Error(`${name}: no numeric or boolean values`);
    }
    const average = mean(numbers);
    if (average < threshold) {
      throw new Error(`${name}: mean ${average.toFixed(3)} is below ${String(threshold)}`);
    }
  }

  /** The score values of scorer `name`, one per run that has one. */
  #values(name: string): (boolean | number | string)[] {
    const values: (boolean | number | string)[] = [];
    for (const run of this.runs) {
      const score = scoreOf(run, name);
      if (score !== undefined) {
        values.push(score.value);
      }
    }
    return values;
  }

  #errorsOf(name: string): number {
    return Object.hasOwn(this.scorerErrors, name) ? (this.scorerErrors[name] ?? 0) : 0;
  }
}

/**
 * Run `task` on every item of a dataset version, score each run with
 * `scorers` and store the results on the server. Raises `ConfigError` before
 * `init()`, `RangeError`/`TypeError` for bad arguments, `ApiError` when the API
 * refuses a call (the create, or a finish that fails).
 */
export async function evaluate(options: EvaluateOptions): Promise<ExperimentResult> {
  const runtime = currentRuntime();
  if (runtime === undefined) {
    throw new ConfigError("evaluate", "needs llmobs.init() with an API key");
  }
  const checked = checkOptions(options);
  const client =
    options.client ??
    new Client({ apiKey: runtime.config.apiKey, baseUrl: runtime.config.baseUrl });
  if (runtime.config.sampleRate < 1) {
    warnOnce(
      "evaluate-sample-rate",
      "llmobs: sample_rate is below 1; some experiment runs will have no trace and no score",
    );
  }
  const created = await createExperiment(
    client,
    options,
    checked.repetitions,
    await defaultNameSource(client, options),
  );
  const items = await readItems(client, created);
  return execute({
    runtime,
    client,
    experiment: created.experiment,
    items,
    repetitions: checked.repetitions,
    concurrency: checked.concurrency,
    task: options.task,
    scorers: checked.scorers,
  });
}

/** A scorer with its name and position, as `evaluate` calls it. */
interface NamedScorer {
  readonly name: string;
  readonly index: number;
  readonly fn: Scorer;
}

/** The argument checks of § `evaluate` flow step 1. Throws; never calls the API. */
function checkOptions(options: EvaluateOptions): {
  concurrency: number;
  repetitions: number;
  scorers: NamedScorer[];
} {
  const { dataset, task, experimentName, datasetVersion } = options;
  if (typeof dataset !== "string" && !(dataset instanceof Dataset)) {
    throw new TypeError("dataset: must be a name or a Dataset");
  }
  if (typeof task !== "function") {
    throw new TypeError("task: must be a function");
  }
  const concurrency = options.concurrency ?? DEFAULT_CONCURRENCY;
  if (!Number.isInteger(concurrency) || concurrency < 1 || concurrency > MAX_CONCURRENCY) {
    throw new RangeError(`concurrency: must be 1 to ${String(MAX_CONCURRENCY)}`);
  }
  const repetitions = options.repetitions ?? 1;
  if (!Number.isInteger(repetitions) || repetitions < 1 || repetitions > MAX_REPETITIONS) {
    throw new RangeError(`repetitions: must be 1 to ${String(MAX_REPETITIONS)}`);
  }
  if (experimentName !== undefined) {
    const length = [...experimentName].length;
    if (length < 1 || length > MAX_NAME_CHARS) {
      throw new RangeError(`experimentName: must be 1 to ${String(MAX_NAME_CHARS)} characters`);
    }
  }
  if (datasetVersion !== undefined && (!Number.isInteger(datasetVersion) || datasetVersion < 0)) {
    throw new RangeError("datasetVersion: must be a non-negative integer");
  }
  const scorers: NamedScorer[] = [];
  const names = new Set<string>();
  (options.scorers ?? []).forEach((fn, index) => {
    if (typeof fn !== "function") {
      throw new TypeError(`scorers[${String(index)}]: must be a function`);
    }
    if (fn.name === "") {
      throw new TypeError(`scorers[${String(index)}]: needs a name`);
    }
    if (names.has(fn.name)) {
      throw new TypeError(`scorers[${String(index)}]: name is used by another scorer`);
    }
    names.add(fn.name);
    scorers.push({ name: fn.name, index, fn });
  });
  return { concurrency, repetitions, scorers };
}

/** The `POST /experiments` answer (D-J-10, `CreateExperimentResponse`). */
interface WireExperiment {
  id: string;
  name: string;
  /** Always set on a new experiment: its dataset exists when it is created. */
  dataset_id: string;
  dataset_version: number;
}

interface WireCreated {
  experiment: WireExperiment;
  items: WireItem[];
  next_cursor: string | null;
}

/**
 * The dataset name the default experiment name needs: a `Dataset` has it; a
 * name given as a string is read with one `GET` (only when no experiment name
 * is given). A string id is read the same way.
 */
async function defaultNameSource(
  client: Client,
  options: EvaluateOptions,
): Promise<string | undefined> {
  if (options.experimentName !== undefined) {
    return undefined;
  }
  const { dataset } = options;
  if (typeof dataset !== "string") {
    return dataset.name;
  }
  const wire = await client.request<{ name: string }>(
    "GET",
    "/datasets/{dataset}",
    `/datasets/${segment(dataset)}`,
  );
  return wire.name;
}

/** Create the experiment. Sent once: a retry could create a second one. */
async function createExperiment(
  client: Client,
  options: EvaluateOptions,
  repetitions: number,
  datasetName: string | undefined,
): Promise<WireCreated> {
  const { dataset } = options;
  const datasetRef = typeof dataset === "string" ? dataset : dataset.id;
  const body: Record<string, unknown> = {
    name: options.experimentName ?? defaultName(datasetName ?? ""),
    dataset: datasetRef,
    repetitions,
  };
  const version =
    options.datasetVersion ?? (typeof dataset === "string" ? undefined : dataset.version);
  if (version !== undefined) {
    body.dataset_version = version;
  }
  if (options.description !== undefined) {
    body.description = options.description;
  }
  if (options.metadata !== undefined) {
    body.metadata = toJsonable(options.metadata);
  }
  return client.request<WireCreated>("POST", "/experiments", "/experiments", {
    body,
    retry: false,
  });
}

/** The first page from the create answer, then the rest of the pinned version (D-J-10). */
async function readItems(client: Client, created: WireCreated): Promise<DatasetItem[]> {
  const items = created.items.map(itemFromWire);
  const { dataset_id: datasetId, dataset_version: version } = created.experiment;
  let cursor = created.next_cursor;
  while (cursor !== null) {
    const page = await client.request<WirePage<WireItem>>(
      "GET",
      "/datasets/{dataset}/items",
      `/datasets/${segment(datasetId)}/items`,
      { query: { version, limit: ITEM_PAGE_SIZE, cursor } },
    );
    items.push(...page.data.map(itemFromWire));
    cursor = page.next_cursor;
  }
  return items;
}

interface Plan {
  readonly runtime: Runtime;
  readonly client: Client;
  readonly experiment: WireExperiment;
  readonly items: readonly DatasetItem[];
  readonly repetitions: number;
  readonly concurrency: number;
  readonly task: (input: unknown, item: DatasetItem) => unknown;
  readonly scorers: readonly NamedScorer[];
}

/** One result row as `POST /experiments/{id}/items` takes it (snake_case). */
interface ResultRow {
  dataset_item_id: string;
  repetition: number;
  latency_ns: number;
  trace_id?: string;
  output?: unknown;
  error?: string;
}

/** Runs the (item, repetition) pairs, at most `concurrency` at once, then finishes. */
async function execute(plan: Plan): Promise<ExperimentResult> {
  const { runtime, client, experiment, items, repetitions } = plan;
  const total = items.length * repetitions;
  const runs = new Array<Run | undefined>(total);
  const uploader = new Uploader(client, experiment.id);
  const scorerErrors = new Map<string, number>();
  const warned = new Set<number>();
  let next = 0;
  const worker = async (): Promise<void> => {
    // An upload failure stops new runs (§ evaluate flow, step 6).
    while (uploader.failure === undefined && next < total) {
      const index = next;
      next += 1;
      const item = items[Math.floor(index / repetitions)];
      if (item === undefined) {
        return;
      }
      const outcome = await runOne(plan, item, index % repetitions, scorerErrors, warned);
      runs[index] = outcome.run;
      await uploader.add(outcome.row, outcome.bytes);
    }
  };
  await Promise.all(Array.from({ length: Math.min(plan.concurrency, total) }, () => worker()));
  await uploader.drain();
  await runtime.flush();
  const failure = uploader.failure;
  if (failure !== undefined) {
    try {
      await client.request("POST", "/experiments/{experiment}/finish", finishPath(experiment), {
        body: { error: UPLOAD_FAILED },
        retry: true,
      });
    } catch (finishError) {
      debug(`llmobs: finishing a failed experiment failed: ${errorName(finishError)}`);
    }
    throw failure;
  }
  await client.request("POST", "/experiments/{experiment}/finish", finishPath(experiment), {
    body: {},
    retry: true,
  });
  return new ExperimentResult({
    experimentId: experiment.id,
    experimentName: experiment.name,
    datasetId: experiment.dataset_id,
    datasetVersion: experiment.dataset_version,
    runs: runs.filter((run): run is Run => run !== undefined),
    scorerErrors: Object.fromEntries(scorerErrors),
    scorerNames: plan.scorers.map((scorer) => scorer.name),
  });
}

function finishPath(experiment: WireExperiment): string {
  return `/experiments/${segment(experiment.id)}/finish`;
}

interface Outcome {
  readonly run: Run;
  readonly row: ResultRow;
  /** The UTF-8 size of the row's JSON output (0 when there is none). */
  readonly bytes: number;
}

/** One run: the task in the experiment scope, then the scorers (§ evaluate flow, step 5). */
async function runOne(
  plan: Plan,
  item: DatasetItem,
  repetition: number,
  scorerErrors: Map<string, number>,
  warned: Set<number>,
): Promise<Outcome> {
  const { experiment, task } = plan;
  let traceId: string | undefined;
  let latencyNs = 0;
  let output: unknown;
  let error: string | undefined;
  const callTask = async (span: SpanHandle): Promise<unknown> => {
    traceId = sampledTraceId(span);
    const started = clockNs();
    let value: unknown;
    try {
      value = await task(item.input, item);
    } finally {
      latencyNs = Math.round(clockNs() - started);
    }
    if (value !== undefined) {
      span.setOutput(value);
    }
    return value;
  };
  try {
    output = await context.with(experimentContext(experiment.id, item.id), () =>
      withSpan(experiment.name, { kind: "chain", input: item.input }, callTask),
    );
  } catch (caught) {
    error = errorText(caught);
  }
  const row: ResultRow = { dataset_item_id: item.id, repetition, latency_ns: latencyNs };
  if (traceId !== undefined) {
    row.trace_id = traceId;
  }
  let bytes = 0;
  let scores: Record<string, Score> = {};
  if (error !== undefined) {
    row.error = error;
  } else {
    const converted = outputForRow(output);
    if (converted !== undefined) {
      row.output = converted.output;
      bytes = converted.bytes;
    }
    scores = await scoreRun(plan, item, output, traceId, scorerErrors, warned);
  }
  const run: Run = {
    datasetItemId: item.id,
    repetition,
    input: item.input,
    expectedOutput: item.expectedOutput,
    output: error === undefined ? output : undefined,
    error,
    traceId,
    latencyNs,
    scores,
  };
  return { run, row, bytes };
}

/** Run the scorers in order, queue their scores when the run has a trace, and return the run's scores. */
async function scoreRun(
  plan: Plan,
  item: DatasetItem,
  output: unknown,
  traceId: string | undefined,
  scorerErrors: Map<string, number>,
  warned: Set<number>,
): Promise<Record<string, Score>> {
  const entries: [string, Score][] = [];
  for (const scorer of plan.scorers) {
    const args: ScorerArgs = {
      input: item.input,
      output,
      expected: item.expectedOutput,
      metadata: item.metadata,
      traceId,
      item,
    };
    const outcome = await callScorer(scorer.fn, args);
    if (outcome.kind === "error") {
      scorerErrors.set(scorer.name, (scorerErrors.get(scorer.name) ?? 0) + 1);
      if (!warned.has(scorer.index)) {
        warned.add(scorer.index);
        warn(`llmobs: scorer ${String(scorer.index)} failed (${outcome.name})`);
      }
      continue;
    }
    if (outcome.kind === "none") {
      continue;
    }
    entries.push([scorer.name, outcome.score]);
    if (traceId !== undefined) {
      if (plan.runtime.scores.paused()) {
        warnOnce(
          "evaluate-score-pause",
          "llmobs: the monthly units cap paused score uploads; experiment scores are being dropped",
        );
      }
      queueExperimentScore(plan.runtime, {
        name: scorer.name,
        value: outcome.score.value,
        traceId,
        experimentId: plan.experiment.id,
        datasetItemId: item.id,
        reason: outcome.score.reason,
        dataType: outcome.score.dataType,
        metadata: outcome.score.metadata,
      });
    }
  }
  return Object.fromEntries(entries);
}

type ScorerOutcome =
  | { kind: "score"; score: Score }
  | { kind: "none" }
  | { kind: "error"; name: string };

/** Call one scorer; a throw or an invalid return value is an error with the type name only. */
async function callScorer(scorer: Scorer, args: ScorerArgs): Promise<ScorerOutcome> {
  let raw: unknown;
  try {
    raw = await scorer(args);
  } catch (caught) {
    return { kind: "error", name: errorName(caught) };
  }
  return normalizeScore(raw);
}

const INVALID_SCORE: ScorerOutcome = { kind: "error", name: "TypeError" };

/** § Scorers, return value: a bare value, a `Score`, nothing, or an error. */
function normalizeScore(raw: unknown): ScorerOutcome {
  if (raw === undefined || raw === null) {
    return { kind: "none" };
  }
  if (typeof raw === "boolean" || typeof raw === "string") {
    return { kind: "score", score: { value: raw } };
  }
  if (typeof raw === "number") {
    return Number.isFinite(raw) ? { kind: "score", score: { value: raw } } : INVALID_SCORE;
  }
  if (typeof raw !== "object" || !("value" in raw)) {
    return INVALID_SCORE;
  }
  const fields = raw as {
    value: unknown;
    reason?: unknown;
    dataType?: unknown;
    metadata?: unknown;
  };
  const value = fields.value;
  if (typeof value !== "boolean" && typeof value !== "string" && typeof value !== "number") {
    return INVALID_SCORE;
  }
  const dataType =
    typeof fields.dataType === "string" ? (fields.dataType as ScoreDataType) : undefined;
  if (valueReason(value, dataType) !== undefined) {
    return INVALID_SCORE;
  }
  const score: Score = { value };
  if (typeof fields.reason === "string") {
    score.reason = fields.reason;
  }
  if (dataType !== undefined) {
    score.dataType = dataType;
  }
  if (
    typeof fields.metadata === "object" &&
    fields.metadata !== null &&
    !Array.isArray(fields.metadata)
  ) {
    score.metadata = fields.metadata as Record<string, unknown>;
  }
  return { kind: "score", score };
}

/**
 * The trace id of a run's root span, or `undefined` when the span is not
 * sampled (its trace has no spans and gets no score).
 */
function sampledTraceId(span: SpanHandle): string | undefined {
  const active = trace.getActiveSpan()?.spanContext();
  return active !== undefined && (active.traceFlags & TraceFlags.SAMPLED) !== 0
    ? span.traceId
    : undefined;
}

/**
 * The row's output (§ Output): the converted value, or its text cut to
 * 260,000 bytes and marked when the text is over 256 KiB. `bytes` is the UTF-8
 * size of the JSON form, the upload budget's unit.
 */
function outputForRow(value: unknown): { output: unknown; bytes: number } | undefined {
  if (value === undefined) {
    return undefined;
  }
  try {
    const converted = toJsonable(value);
    const text = JSON.stringify(converted);
    const size = utf8Encode(text).length;
    if (size <= OUTPUT_LIMIT_BYTES) {
      return { output: converted, bytes: size };
    }
    const kept = cutUtf8(text, OUTPUT_KEEP_BYTES);
    const truncated = `${kept}…[llmobs: truncated, original ${String(size)} bytes]`;
    return { output: truncated, bytes: utf8Encode(JSON.stringify(truncated)).length };
  } catch {
    const marker = `<unserializable ${typeName(value)}>`;
    return { output: marker, bytes: utf8Encode(JSON.stringify(marker)).length };
  }
}

/** The longest prefix of `text` within `limit` UTF-8 bytes, on a character boundary. */
function cutUtf8(text: string, limit: number): string {
  const bytes = utf8Encode(text);
  let cut = limit;
  while (cut > 0 && ((bytes[cut] ?? 0) & 0xc0) === 0x80) {
    cut -= 1;
  }
  return utf8Decode(bytes.subarray(0, cut));
}

/**
 * Batches the result rows (§ evaluate flow, step 6). A batch is posted when
 * 500 rows or 32 MiB of outputs are waiting, and at the end. Posts are upserts
 * (retried) and run one at a time. The first failure is kept; later posts are
 * skipped.
 */
class Uploader {
  readonly #client: Client;
  readonly #path: string;
  #rows: ResultRow[] = [];
  #bytes = 0;
  #chain: Promise<void> = Promise.resolve();
  #failure: ApiError | undefined;

  constructor(client: Client, experimentId: string) {
    this.#client = client;
    this.#path = `/experiments/${segment(experimentId)}/items`;
  }

  get failure(): ApiError | undefined {
    return this.#failure;
  }

  /** Queue one row; resolves once every post sent so far has settled. */
  async add(row: ResultRow, bytes: number): Promise<void> {
    if (this.#rows.length > 0 && this.#bytes + bytes > UPLOAD_BATCH_BYTES) {
      this.#post(this.#take());
    }
    this.#rows.push(row);
    this.#bytes += bytes;
    if (this.#rows.length >= UPLOAD_BATCH_ROWS) {
      this.#post(this.#take());
    }
    await this.#chain;
  }

  /** Post the rows still waiting, then resolve once every post has settled. */
  async drain(): Promise<void> {
    if (this.#rows.length > 0) {
      this.#post(this.#take());
    }
    await this.#chain;
  }

  #take(): ResultRow[] {
    const rows = this.#rows;
    this.#rows = [];
    this.#bytes = 0;
    return rows;
  }

  #post(rows: ResultRow[]): void {
    this.#chain = this.#chain.then(async () => {
      if (this.#failure !== undefined) {
        return;
      }
      try {
        await this.#client.request("POST", "/experiments/{experiment}/items", this.#path, {
          body: { results: rows },
          retry: true,
        });
      } catch (error) {
        // `request` raises only ApiError; the first one stops the run.
        if (error instanceof ApiError) {
          this.#failure = error;
        }
      }
    });
  }
}

/** The score of scorer `name` in a run; own properties only, so `constructor` finds nothing. */
function scoreOf(run: Run, name: string): Score | undefined {
  return Object.hasOwn(run.scores, name) ? run.scores[name] : undefined;
}

function mean(values: readonly number[]): number {
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}

/**
 * "label count" pairs: counts descending, ties by label. Interpretation of
 * the spec's "labels sorted, counts descending".
 */
function labelCounts(labels: readonly string[]): string {
  const counts = new Map<string, number>();
  for (const label of labels) {
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  return [...counts.entries()]
    .sort(([labelA, countA], [labelB, countB]) =>
      countA !== countB ? countB - countA : labelA < labelB ? -1 : labelA > labelB ? 1 : 0,
    )
    .map(([label, count]) => `${label} ${String(count)}`)
    .join(", ");
}

/** `YYYY-MM-DD HH:MM:SS` in UTC. */
function defaultName(datasetName: string): string {
  return `${datasetName} ${new Date().toISOString().slice(0, 19).replace("T", " ")}`;
}

/** Monotonic nanoseconds (wall clock in milliseconds where there is no `performance`). */
function clockNs(): number {
  const { performance } = runtimeGlobals();
  return (performance !== undefined ? performance.now() : Date.now()) * 1_000_000;
}

/** `<ExceptionTypeName>: <message>`, cut to 4,096 UTF-16 code units. */
function errorText(caught: unknown): string {
  const text =
    caught instanceof Error ? `${caught.name}: ${caught.message}` : `Error: ${String(caught)}`;
  return text.slice(0, ERROR_LIMIT_CHARS);
}

function typeName(value: unknown): string {
  try {
    return (value as { constructor?: { name?: string } } | null)?.constructor?.name ?? typeof value;
  } catch {
    return typeof value;
  }
}

function errorName(error: unknown): string {
  return error instanceof Error ? error.name : typeof error;
}
