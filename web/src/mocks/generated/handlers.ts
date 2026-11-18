import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import {
  type Dataset,
  type UserGroup,
  userSummaries,
  visibleTraces,
} from "@/mocks/generated/dataset";
import {
  createDatasets,
  type GeneratedDataset,
  type GeneratedExperiment,
  itemsAt,
} from "@/mocks/generated/datasets";
import { experimentHandlers } from "@/mocks/generated/experiments";
import { widgetQueryHandler } from "@/mocks/generated/widget-query";
import {
  type SpanStreamSource,
  type StreamEmit,
  spanStreamHandler,
  type TraceStreamSource,
  traceStreamHandler,
} from "@/mocks/sse";

const PROJECT = "/api/v1/projects/:project";

type Order = Schema<"SortOrder">;

const DEFINED_AT = "2026-01-01T09:00:00.000000000Z";

/** Definitions of the generated scores, both "higher is better", so lists show their tones. */
const SCORE_DEFINITIONS: Schema<"ScoreDefinition">[] = [
  {
    id: "01900000-0000-7000-8000-000000000611",
    name: "correctness",
    data_type: "numeric",
    categories: [],
    min_value: 0,
    max_value: 1,
    description: "How correct the answer is, from 0 to 1.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000000612",
    name: "relevance",
    data_type: "boolean",
    categories: [],
    description: "Whether the answer is relevant.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000000613",
    name: "exact",
    data_type: "boolean",
    categories: [],
    description: "Whether the answer matches the expected output.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000000614",
    name: "quality",
    data_type: "numeric",
    categories: [],
    min_value: 0,
    max_value: 1,
    description: "How good the answer is, from 0 to 1.",
    direction: "higher",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
  {
    id: "01900000-0000-7000-8000-000000000615",
    name: "tone",
    data_type: "categorical",
    categories: [
      { label: "neutral" },
      { label: "friendly" },
      { label: "formal" },
      { label: "curt" },
    ],
    description: "The tone of the answer.",
    direction: "none",
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  },
];

export function largeHandlers(dataset: Dataset): HttpHandler[] {
  return [
    widgetQueryHandler(dataset),
    traceStreamHandler(liveTraceSource(dataset)),
    spanStreamHandler(liveSpanSource(dataset)),
    http.get(`${PROJECT}/traces`, ({ request }) => listTraces(dataset, request)),
    http.get(`${PROJECT}/traces/:trace_id/spans/:span_id`, ({ params }) =>
      oneSpan(dataset, params.trace_id, params.span_id),
    ),
    http.get(`${PROJECT}/traces/:trace_id`, ({ params }) => oneTrace(dataset, params.trace_id)),
    http.get(`${PROJECT}/spans`, ({ request }) => listSpans(dataset, request)),
    http.get(`${PROJECT}/sessions/:session_id`, ({ request, params }) =>
      oneSession(dataset, request, params.session_id),
    ),
    http.get(`${PROJECT}/sessions`, ({ request }) => listSessions(dataset, request)),
    http.get(`${PROJECT}/users/:user_id`, ({ request, params }) =>
      oneUser(dataset, request, params.user_id),
    ),
    http.get(`${PROJECT}/users`, ({ request }) => listUsers(dataset, request)),
    http.get(`${PROJECT}/metrics`, ({ request }) => metrics(dataset, request)),
    http.get(`${PROJECT}/facets`, ({ request }) => facets(dataset, request)),
    http.get(`${PROJECT}/score-definitions`, () =>
      HttpResponse.json({ data: SCORE_DEFINITIONS, next_cursor: null }),
    ),
    http.get(`${PROJECT}/dataset-trends`, ({ request }) => datasetTrends(dataset, request)),
    http.get(`${PROJECT}/datasets`, ({ request }) => listDatasets(dataset, request)),
    http.get(`${PROJECT}/datasets/:dataset/items/:item_id`, ({ params }) =>
      oneDatasetItem(dataset, params.dataset, params.item_id),
    ),
    http.get(`${PROJECT}/datasets/:dataset/items`, ({ request, params }) =>
      listDatasetItems(dataset, request, params.dataset),
    ),
    http.get(`${PROJECT}/datasets/:dataset/versions/:version/changes`, ({ request, params }) =>
      listVersionChanges(dataset, request, params.dataset, params.version),
    ),
    http.get(`${PROJECT}/datasets/:dataset/versions`, ({ request, params }) =>
      listDatasetVersions(dataset, request, params.dataset),
    ),
    http.get(`${PROJECT}/datasets/:dataset`, ({ params }) => oneDataset(dataset, params.dataset)),
    http.get(`${PROJECT}/experiments`, ({ request }) => listExperiments(dataset, request)),
    http.get(`${PROJECT}/experiments/:id/results`, ({ request, params }) =>
      experimentResults(dataset, request, params.id),
    ),
    http.get(`${PROJECT}/experiments/:id`, ({ params }) => oneExperiment(dataset, params.id)),
    // The experiments of the generated datasets answer above; every other experiment request falls
    // through to the experiments mock (its runs are on the fixture dataset).
    ...experimentHandlers(),
    http.get(`${PROJECT}/experiments/summaries`, ({ request }) =>
      experimentSummaries(dataset, request),
    ),
  ];
}

/** The seed of the generated datasets. The trace dataset's seed is not kept, so this one is fixed. */
const DATASET_SEED = 1;

type DatasetIndex = {
  /** By name, the server's order (`lower(name), id`). */
  datasets: GeneratedDataset[];
  byId: Map<string, GeneratedDataset>;
  byName: Map<string, GeneratedDataset>;
  /** Every experiment, newest first (the server's `id DESC`). */
  experiments: GeneratedExperiment[];
  experimentById: Map<string, GeneratedExperiment>;
};

const datasetIndexes = new WeakMap<Dataset, DatasetIndex>();

/** The generated datasets of a trace dataset, built once. They share its `now`. */
function indexOf(dataset: Dataset): DatasetIndex {
  const cached = datasetIndexes.get(dataset);
  if (cached) {
    return cached;
  }
  const generated = createDatasets({
    seed: DATASET_SEED,
    now: dataset.now,
    traces: dataset.traces,
  });
  const datasets = [...generated].sort((left, right) => {
    const byName = left.dataset.name.toLowerCase().localeCompare(right.dataset.name.toLowerCase());
    return byName !== 0 ? byName : left.dataset.id.localeCompare(right.dataset.id);
  });
  const experiments = generated
    .flatMap((item) => item.experiments)
    .sort((left, right) => right.experiment.id.localeCompare(left.experiment.id));
  const index: DatasetIndex = {
    datasets,
    byId: new Map(datasets.map((item) => [item.dataset.id, item])),
    byName: new Map(datasets.map((item) => [item.dataset.name, item])),
    experiments,
    experimentById: new Map(experiments.map((item) => [item.experiment.id, item])),
  };
  datasetIndexes.set(dataset, index);
  return index;
}

/** A dataset by id or name, as the server's routes take it. */
function findDataset(index: DatasetIndex, raw: string): GeneratedDataset | undefined {
  return index.byId.get(raw) ?? index.byName.get(raw);
}

function listDatasets(dataset: Dataset, request: Request): Response {
  const url = new URL(request.url);
  const parsed = readQuery(url);
  if (parsed instanceof Response) {
    return parsed;
  }
  const needle = (url.searchParams.get("q") ?? "").trim().toLowerCase();
  const rows = indexOf(dataset)
    .datasets.filter((item) => needle === "" || item.dataset.name.toLowerCase().includes(needle))
    .map((item) => item.dataset);
  return page(rows, parsed, {});
}

function oneDataset(
  dataset: Dataset,
  datasetParam: string | readonly string[] | undefined,
): Response {
  const found = findDataset(indexOf(dataset), text(datasetParam));
  if (!found) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(found.dataset);
}

function listDatasetItems(
  dataset: Dataset,
  request: Request,
  datasetParam: string | readonly string[] | undefined,
): Response {
  const url = new URL(request.url);
  const parsed = readQuery(url);
  if (parsed instanceof Response) {
    return parsed;
  }
  const found = findDataset(indexOf(dataset), text(datasetParam));
  if (!found) {
    return problem(404, "not_found", "Not Found");
  }
  // Items of the requested version: those introduced at or before it (default: the latest).
  const raw = url.searchParams.get("version");
  if (raw !== null && !/^\d+$/.test(raw)) {
    return problem(400, "bad_request", "Bad Request");
  }
  const search = readSearch(url.searchParams.get("q"));
  if (search instanceof Response) {
    return search;
  }
  const version = raw === null ? found.dataset.current_version : Number(raw);
  return page(
    itemsAt(found, version).filter(
      (item) => search === undefined || searchText(item).includes(search),
    ),
    parsed,
    {},
  );
}

/** The item search (D-J-22): trimmed, 1 to 200 characters, else a 400 on `q`. Lower-cased. */
function readSearch(raw: string | null): string | undefined | Response {
  if (raw === null) {
    return undefined;
  }
  const trimmed = raw.trim();
  if ([...trimmed].length < 1 || [...trimmed].length > 200) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "q", message: "must be 1 to 200 characters" },
    ]);
  }
  return trimmed.toLowerCase();
}

/** The JSON text of an item's input, expected output and metadata, lower-cased, as the search reads it. */
function searchText(item: Schema<"DatasetItem">): string {
  return [
    JSON.stringify(item.input),
    item.expected_output === undefined ? "" : JSON.stringify(item.expected_output),
    JSON.stringify(item.metadata),
  ]
    .join(" ")
    .toLowerCase();
}

function oneDatasetItem(
  dataset: Dataset,
  datasetParam: string | readonly string[] | undefined,
  itemParam: string | readonly string[] | undefined,
): Response {
  const found = findDataset(indexOf(dataset), text(datasetParam));
  const item = found?.items.find((row) => row.id === text(itemParam));
  if (!item) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(item);
}

function listDatasetVersions(
  dataset: Dataset,
  request: Request,
  datasetParam: string | readonly string[] | undefined,
): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const found = findDataset(indexOf(dataset), text(datasetParam));
  if (!found) {
    return problem(404, "not_found", "Not Found");
  }
  return page([...found.versions].reverse(), parsed, {});
}

/**
 * `GET …/datasets/{dataset}/versions/{version}/changes` (D-J-24): the version's history.
 */
function listVersionChanges(
  dataset: Dataset,
  request: Request,
  datasetParam: string | readonly string[] | undefined,
  versionParam: string | readonly string[] | undefined,
): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const found = findDataset(indexOf(dataset), text(datasetParam));
  if (!found) {
    return problem(404, "not_found", "Not Found");
  }
  const raw = text(versionParam);
  if (!/^\d+$/.test(raw)) {
    return problem(400, "bad_request", "Bad Request");
  }
  const version = Number(raw);
  if (version < 1 || version > found.dataset.current_version) {
    return problem(404, "not_found", "Not Found");
  }
  const changes: Schema<"ItemChange">[] = found.history
    .filter((change) => change.version === version)
    .sort((left, right) =>
      left.item_id < right.item_id ? -1 : left.item_id > right.item_id ? 1 : 0,
    )
    .map((change) => ({
      item_id: change.item_id,
      change: change.change,
      ...(change.before === undefined ? {} : { before: change.before }),
      ...(change.after === undefined ? {} : { after: change.after }),
    }));
  return page(changes, parsed, {});
}

/**
 * The experiments of one generated dataset. Without `dataset`, or for a dataset the generator did
 * not make, it answers nothing, so the experiments mock (`experimentHandlers`) answers instead.
 */
function listExperiments(dataset: Dataset, request: Request): Response | undefined {
  const url = new URL(request.url);
  const parsed = readQuery(url);
  if (parsed instanceof Response) {
    return parsed;
  }
  const index = indexOf(dataset);
  const datasetParam = url.searchParams.get("dataset");
  const found = datasetParam === null ? undefined : findDataset(index, datasetParam);
  if (!found) {
    return undefined;
  }
  const rows = index.experiments.filter((item) => item.experiment.dataset_id === found.dataset.id);
  return page(
    rows.map((item) => item.experiment),
    parsed,
    {},
  );
}

function oneExperiment(
  dataset: Dataset,
  idParam: string | readonly string[] | undefined,
): Response | undefined {
  // Not a generated experiment: the experiments mock answers.
  const found = indexOf(dataset).experimentById.get(text(idParam));
  return found ? HttpResponse.json(found.experiment) : undefined;
}

/** Most datasets one trends request names, and experiments per dataset in its answer (D-J-26). */
const MAX_TREND_DATASETS = 100;
const TRENDS_PER_DATASET = 10;
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** `GET …/dataset-trends?ids=` (D-J-26): the latest experiments of each dataset that exists. */
function datasetTrends(dataset: Dataset, request: Request): Response {
  const ids = readTrendIds(new URL(request.url).searchParams.get("ids"));
  if (ids instanceof Response) {
    return ids;
  }
  const index = indexOf(dataset);
  const data = ids.flatMap((id) => {
    const found = index.byId.get(id);
    if (!found) {
      return [];
    }
    return [
      {
        dataset_id: found.dataset.id,
        experiments: found.experiments
          .slice(-TRENDS_PER_DATASET)
          .reverse()
          .map((item) => trendOf(item)),
      },
    ];
  });
  return HttpResponse.json({ data, scores_unavailable: false });
}

/** The ids of a trends request: 1 to 100 distinct UUIDs, comma-separated, lower-cased. */
function readTrendIds(raw: string | null): string[] | Response {
  const ids = (raw ?? "").split(",").map((part) => part.trim().toLowerCase());
  if (!ids.every((id) => UUID_PATTERN.test(id))) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: "must be comma-separated UUIDs" },
    ]);
  }
  if (ids.length < 1 || ids.length > MAX_TREND_DATASETS) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: `must hold 1 to ${MAX_TREND_DATASETS} ids` },
    ]);
  }
  if (new Set(ids).size !== ids.length) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "ids", message: "must be distinct" },
    ]);
  }
  return ids;
}

/** One experiment of a trends answer: its summary's error count and its numeric and boolean scores. */
function trendOf(generated: GeneratedExperiment): Schema<"ExperimentTrend"> {
  const { experiment, summary } = generated;
  return {
    id: experiment.id,
    name: experiment.name,
    status: experiment.status,
    created_at: experiment.created_at,
    result_count: experiment.result_count,
    error_count: summary.error_count,
    scores: summary.scores.map((score) => ({
      name: score.name,
      data_type: score.data_type,
      count: score.count,
      // A boolean's mean is its pass rate (D-J-26).
      ...(score.data_type === "boolean"
        ? score.pass_rate === undefined
          ? {}
          : { mean: score.pass_rate }
        : score.mean === undefined
          ? {}
          : { mean: score.mean }),
    })),
  };
}

/** `GET …/experiments/{id}/results`: the summary, and one page of rows in item then repetition order. */
/** `GET /experiments/summaries?ids=` for the generated experiments (after the experiments mock). */
function experimentSummaries(dataset: Dataset, request: Request): Response {
  const byId = indexOf(dataset).experimentById;
  const ids = (new URL(request.url).searchParams.get("ids") ?? "").split(",").filter(Boolean);
  const data = ids.flatMap((id) => {
    const found = byId.get(id);
    return found ? [found.summary] : [];
  });
  return HttpResponse.json({ data, skipped: [] });
}

function experimentResults(
  dataset: Dataset,
  request: Request,
  idParam: string | readonly string[] | undefined,
): Response | undefined {
  const found = indexOf(dataset).experimentById.get(text(idParam));
  if (!found) {
    // Not a generated experiment: the experiments mock answers.
    return undefined;
  }
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const end = parsed.offset + parsed.limit;
  return HttpResponse.json({
    summary: found.summary,
    rows: found.rows.slice(parsed.offset, end),
    next_cursor: end < found.rows.length ? encodeCursor(end) : null,
  });
}

function listTraces(dataset: Dataset, request: Request): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const rows = sortTraces(filterTraces(dataset.traces, parsed), parsed.sort, parsed.order);
  const filtered = userIdOfFilter(parsed.filter) !== undefined;
  return page(rows, parsed, filtered ? {} : ignoredFilter(parsed));
}

function listSpans(dataset: Dataset, request: Request): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const traces = sortTraces(filterTraces(dataset.traces, parsed), "start_time", parsed.order);
  if (parsed.sort !== "start_time") {
    const spans = traces.flatMap((trace) => spansOf(dataset, trace.trace_id, "asc"));
    return page(
      sortSpans(filterSpans(spans, parsed), parsed.sort, parsed.order),
      parsed,
      userIdOfFilter(parsed.filter) === undefined ? ignoredFilter(parsed) : {},
    );
  }
  return pageSpans(dataset, traces, parsed);
}

function oneTrace(dataset: Dataset, traceId: string | readonly string[] | undefined): Response {
  const id = text(traceId);
  const detail = extraDetail(dataset, id) ?? dataset.traceDetail(id);
  if (!detail) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(detail);
}

function oneSpan(
  dataset: Dataset,
  traceId: string | readonly string[] | undefined,
  spanId: string | readonly string[] | undefined,
): Response {
  const trace = text(traceId);
  const span = text(spanId);
  const fromExtra = extraDetail(dataset, trace)?.spans.find((row) => row.span.span_id === span);
  const detail = fromExtra ? spanDetailFrom(fromExtra.span) : dataset.spanDetail(trace, span);
  if (!detail) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(detail);
}

function listSessions(dataset: Dataset, request: Request): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const rows = dataset
    .sessions(parsed.includeEval, parsed.includeExperiments)
    .filter((session) => inRange(session.start_time, parsed.from, parsed.to))
    .sort((left, right) => compareSessions(left, right, parsed.sort, parsed.order));
  return page(rows, parsed, ignoredFilter(parsed));
}

function oneSession(
  dataset: Dataset,
  request: Request,
  sessionId: string | readonly string[] | undefined,
): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const detail = dataset.session(text(sessionId), parsed.includeEval, parsed.includeExperiments);
  if (!detail) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(detail);
}

function listUsers(dataset: Dataset, request: Request): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const rows = dataset
    .users(parsed.includeEval, parsed.includeExperiments, { from: parsed.from, to: parsed.to })
    .sort((left, right) => compareUsers(left.user, right.user, parsed.sort, parsed.order));
  // Activity is filled for the returned page only, as the server does (spec 09).
  const window =
    parsed.from !== undefined && parsed.to !== undefined
      ? { from: parsed.from, to: parsed.to }
      : undefined;
  const offset = parsed.offset;
  const next = offset + parsed.limit < rows.length ? encodeCursor(offset + parsed.limit) : null;
  const data = userSummaries(rows.slice(offset, offset + parsed.limit), window);
  return HttpResponse.json({ data, next_cursor: next }, { headers: ignoredFilter(parsed) });
}

function oneUser(
  dataset: Dataset,
  request: Request,
  userId: string | readonly string[] | undefined,
): Response {
  const parsed = readQuery(new URL(request.url));
  if (parsed instanceof Response) {
    return parsed;
  }
  const detail = dataset.user(text(userId), parsed.includeEval, parsed.includeExperiments, {
    from: parsed.from,
    to: parsed.to,
  });
  if (!detail) {
    return problem(404, "not_found", "Not Found");
  }
  return HttpResponse.json(detail);
}

function metrics(dataset: Dataset, request: Request): Response {
  const url = new URL(request.url);
  const parsed = readQuery(url);
  if (parsed instanceof Response) {
    return parsed;
  }
  if (parsed.filter === "bad") {
    return invalidFilter();
  }
  const from = parsed.from;
  const to = parsed.to;
  if (from === undefined || to === undefined) {
    return problem(400, "bad_request", "Bad Request");
  }
  const userId = userIdOfFilter(parsed.filter);
  const body = dataset.metrics(
    from,
    to,
    bucketOf(url.searchParams.get("bucket")),
    parsed.includeEval,
    parsed.includeExperiments,
    userId,
  );
  return HttpResponse.json(body, { headers: userId === undefined ? ignoredFilter(parsed) : {} });
}

/**
 * The facet columns of each view and their kind, as the server's table (D1). A `string_map`
 * column facets its keys (`keys`); `number` columns are histograms.
 */
type FacetColumnKind = "value" | "bool" | "list" | "keys" | "number";

const FACET_COLUMNS: Record<"traces" | "spans" | "sessions", Record<string, FacetColumnKind>> = {
  traces: {
    name: "value",
    status: "value",
    environment: "value",
    release: "value",
    session_id: "value",
    user_id: "value",
    has_root: "bool",
    tags: "list",
    models: "list",
    metadata: "keys",
    duration_ms: "number",
    cost_total: "number",
    total_tokens: "number",
    input_tokens: "number",
    output_tokens: "number",
    span_count: "number",
    error_count: "number",
    llm_count: "number",
  },
  spans: {
    name: "value",
    span_kind: "value",
    status: "value",
    service_name: "value",
    environment: "value",
    release: "value",
    provider: "value",
    model: "value",
    request_model: "value",
    response_model: "value",
    operation: "value",
    cost_source: "value",
    scope_name: "value",
    session_id: "value",
    user_id: "value",
    tags: "list",
    metadata: "keys",
    attributes: "keys",
    resource_attributes: "keys",
    duration_ms: "number",
    cost_total: "number",
    total_tokens: "number",
    input_tokens: "number",
    output_tokens: "number",
  },
  sessions: {
    user_id: "value",
    tags: "list",
    duration_ms: "number",
    trace_count: "number",
    error_count: "number",
    total_tokens: "number",
    cost_total: "number",
  },
};

/** One row of a facet view, flattened to the facet columns. Columns the summaries lack stay absent. */
type FacetRow = Record<string, unknown>;

/** The spans of every trace of a dataset, built once: a spans facet reads all of them. */
const allSpanRows = new WeakMap<Dataset, Schema<"SpanSummary">[]>();

function spansOfDataset(dataset: Dataset): Schema<"SpanSummary">[] {
  const cached = allSpanRows.get(dataset);
  if (cached) {
    return cached;
  }
  const spans = dataset.traces.flatMap((trace) => spansOf(dataset, trace.trace_id, "asc"));
  allSpanRows.set(dataset, spans);
  return spans;
}

function traceRow(trace: Schema<"TraceSummary">): FacetRow {
  return {
    name: trace.name,
    status: trace.status,
    environment: trace.environment,
    release: trace.release,
    session_id: trace.session_id,
    user_id: trace.user_id,
    has_root: trace.has_root,
    tags: trace.tags,
    models: trace.models,
    metadata: trace.metadata,
    duration_ms: trace.duration_ns / 1e6,
    cost_total: trace.cost_total,
    total_tokens: trace.total_tokens,
    input_tokens: trace.input_tokens,
    output_tokens: trace.output_tokens,
    span_count: trace.span_count,
    error_count: trace.error_count,
    llm_count: trace.llm_count,
  };
}

function spanRow(span: Schema<"SpanSummary">): FacetRow {
  return {
    name: span.name,
    span_kind: span.span_kind,
    status: span.status,
    service_name: span.service_name,
    environment: span.environment,
    release: span.release,
    provider: span.provider,
    model: span.model,
    session_id: span.session_id,
    user_id: span.user_id,
    tags: span.tags,
    metadata: span.metadata,
    duration_ms: span.duration_ns / 1e6,
    cost_total: span.cost_total,
    total_tokens: span.total_tokens,
    input_tokens: span.input_tokens,
    output_tokens: span.output_tokens,
  };
}

function sessionRow(session: Schema<"SessionSummary">): FacetRow {
  return {
    user_id: session.user_id,
    tags: session.tags,
    duration_ms: session.duration_ns / 1e6,
    trace_count: session.trace_count,
    error_count: session.error_count,
    total_tokens: session.total_tokens,
    cost_total: session.cost_total,
  };
}

/**
 * The rows a facet view counts, in the list's window and flags. The mock ignores `filter` and
 * `search` (it has no filter engine), so its counts are those of the unfiltered list.
 */
function facetRows(
  dataset: Dataset,
  view: "traces" | "spans" | "sessions",
  query: Query,
): FacetRow[] {
  if (view === "traces") {
    return visibleTraces(dataset.traces, query.includeEval, query.includeExperiments)
      .filter((trace) => inRange(trace.start_time, query.from, query.to))
      .map(traceRow);
  }
  if (view === "sessions") {
    return dataset
      .sessions(query.includeEval, query.includeExperiments)
      .filter((session) => inRange(session.start_time, query.from, query.to))
      .map(sessionRow);
  }
  return spansOfDataset(dataset)
    .filter(
      (span) =>
        (query.includeEval || span.environment !== "cetra-eval") &&
        (query.includeExperiments || span.metadata["cetra.experiment_id"] === undefined) &&
        inRange(span.start_time, query.from, query.to),
    )
    .map(spanRow);
}

/** The field of a facet request: `column` or `map['key']` (D1), else undefined. */
function parseFacetField(
  view: "traces" | "spans" | "sessions",
  raw: string,
): { column: string; key?: string } | undefined {
  const match = /^([A-Za-z_]+)(?:\['((?:[^']|'')*)'\])?$/.exec(raw);
  if (!match) {
    return undefined;
  }
  const column = (match[1] ?? "").toLowerCase();
  const kind: FacetColumnKind | undefined = FACET_COLUMNS[view][column];
  if (kind === undefined) {
    return undefined;
  }
  if (match[2] === undefined) {
    return { column };
  }
  const key = match[2].replaceAll("''", "'");
  if (kind !== "keys" || key.length < 1 || key.length > 256) {
    return undefined;
  }
  return { column, key };
}

/** The distinct values a row holds under a value, list or key field (D1). */
function rowValues(row: FacetRow, column: string, kind: FacetColumnKind, key?: string): string[] {
  const raw = row[column];
  if (key !== undefined) {
    const found = (raw as Record<string, unknown> | undefined)?.[key];
    return typeof found === "string" || typeof found === "number" || typeof found === "boolean"
      ? [String(found)]
      : [];
  }
  if (kind === "keys") {
    return Object.keys((raw as Record<string, unknown> | undefined) ?? {}).filter(
      (name) => !name.startsWith("cetra."),
    );
  }
  if (kind === "list") {
    return Array.isArray(raw) ? [...new Set(raw.map(String))] : [];
  }
  return raw === undefined || raw === null ? [] : [String(raw)];
}

function nearestRank(sorted: readonly number[], fraction: number): number {
  const index = Math.max(0, Math.ceil(fraction * sorted.length) - 1);
  return sorted[Math.min(index, sorted.length - 1)] ?? 0;
}

/** The buckets of D1: 20 equal-width buckets over `[min, p99)`, then `[p99, max]`. */
function bucketsOf(
  numbers: readonly number[],
  min: number,
  max: number,
  p99: number,
): { lo: number; hi: number; count: number }[] {
  if (!(p99 > min)) {
    return [{ lo: min, hi: max, count: numbers.length }];
  }
  const width = (p99 - min) / 20;
  const counts = new Array<number>(21).fill(0);
  for (const value of numbers) {
    if (value >= p99) {
      counts[20] = (counts[20] ?? 0) + 1;
    } else {
      const index = Math.min(19, Math.floor((value - min) / width));
      counts[index] = (counts[index] ?? 0) + 1;
    }
  }
  const buckets = Array.from({ length: 20 }, (_, index) => ({
    lo: min + index * width,
    hi: index === 19 ? p99 : min + (index + 1) * width,
    count: counts[index] ?? 0,
  }));
  if (max >= p99) {
    buckets.push({ lo: p99, hi: max, count: counts[20] ?? 0 });
  }
  return buckets;
}

/** The answer of one facet request over the rows of its view (D1). */
function facetAnswer(
  view: "traces" | "spans" | "sessions",
  field: { column: string; key?: string },
  rows: FacetRow[],
  search: string | undefined,
  limit: number,
): Schema<"FacetsResponse"> {
  const fieldText = field.key === undefined ? field.column : `${field.column}['${field.key}']`;
  const kind = FACET_COLUMNS[view][field.column] ?? "value";
  const total = rows.length;
  if (kind === "number" && field.key === undefined) {
    const numbers = rows
      .map((row) => row[field.column])
      .filter((value): value is number => typeof value === "number" && Number.isFinite(value))
      .sort((left, right) => left - right);
    const base = {
      view,
      field: fieldText,
      kind: "histogram" as const,
      total,
      missing: total - numbers.length,
    };
    const min = numbers[0];
    const max = numbers.at(-1);
    if (min === undefined || max === undefined) {
      return base;
    }
    const p99 = nearestRank(numbers, 0.99);
    return {
      ...base,
      histogram: {
        min,
        max,
        p50: nearestRank(numbers, 0.5),
        p95: nearestRank(numbers, 0.95),
        p99,
        buckets: bucketsOf(numbers, min, max, p99),
      },
    };
  }
  const counts = new Map<string, number>();
  let missing = 0;
  for (const row of rows) {
    const values = rowValues(row, field.column, kind, field.key);
    if (values.length === 0) {
      missing += 1;
    }
    for (const value of values) {
      counts.set(value, (counts.get(value) ?? 0) + 1);
    }
  }
  const needle = search?.toLowerCase();
  const matching = [...counts]
    .filter(([value]) => needle === undefined || value.toLowerCase().includes(needle))
    .sort(([leftValue, leftCount], [rightValue, rightCount]) =>
      rightCount !== leftCount
        ? rightCount - leftCount
        : leftValue < rightValue
          ? -1
          : leftValue > rightValue
            ? 1
            : 0,
    );
  return {
    view,
    field: fieldText,
    kind: field.key === undefined && kind === "keys" ? "keys" : "values",
    total,
    missing,
    values: matching.slice(0, limit).map(([value, count]) => ({ value, count })),
    more: matching.length > limit,
  };
}

/** `GET …/facets` (D1). The mock ignores `filter` and `search`; `q` narrows the values. */
function facets(dataset: Dataset, request: Request): Response {
  const url = new URL(request.url);
  const parsed = readQuery(url);
  if (parsed instanceof Response) {
    return parsed;
  }
  const view = url.searchParams.get("view");
  if (view !== "traces" && view !== "spans" && view !== "sessions") {
    return problem(400, "bad_request", "Bad Request", [
      { field: "view", message: "is not a view" },
    ]);
  }
  const field = parseFacetField(view, url.searchParams.get("field") ?? "");
  if (!field) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "field", message: "field cannot be faceted" },
    ]);
  }
  const rawLimit = url.searchParams.get("limit");
  const limit = rawLimit === null ? 10 : /^\d+$/.test(rawLimit) ? Number(rawLimit) : 0;
  if (limit < 1 || limit > 100) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "limit", message: "must be 1 to 100" },
    ]);
  }
  const rawQuery = url.searchParams.get("q");
  const search = rawQuery === null ? undefined : rawQuery.trim();
  if (search !== undefined && (search.length < 1 || [...search].length > 200)) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "q", message: "must be 1 to 200 characters" },
    ]);
  }
  if (view === "sessions" && url.searchParams.has("search")) {
    return problem(400, "bad_request", "Bad Request", [
      { field: "search", message: "is not supported for sessions" },
    ]);
  }
  return HttpResponse.json(
    facetAnswer(view, field, facetRows(dataset, view, parsed), search, limit),
  );
}

/** The user id of a filter of exactly the form `user_id = '<id>'` (UP-5), else undefined. */
export function userIdOfFilter(filter: string | undefined): string | undefined {
  const match = /^\s*user_id\s*=\s*'((?:[^']|'')*)'\s*$/.exec(filter ?? "");
  return match?.[1]?.replaceAll("''", "'");
}

type Query = {
  from?: number;
  to?: number;
  sort: string;
  order: Order;
  limit: number;
  offset: number;
  includeEval: boolean;
  includeExperiments: boolean;
  search?: string;
  filter?: string;
};

function readQuery(url: URL): Query | Response {
  const params = url.searchParams;
  if (params.get("filter") === "bad") {
    return invalidFilter();
  }
  const limit = readLimit(params.get("limit"));
  if (limit instanceof Response) {
    return limit;
  }
  const offset = readCursor(params.get("cursor"));
  if (offset instanceof Response) {
    return offset;
  }
  const from = readTime(params.get("from"));
  const to = readTime(params.get("to"));
  if (from instanceof Response || to instanceof Response) {
    return problem(400, "bad_request", "Bad Request");
  }
  const order = params.get("order") ?? "desc";
  if (order !== "asc" && order !== "desc") {
    return problem(400, "bad_request", "Bad Request");
  }
  return {
    ...(from === undefined ? {} : { from }),
    ...(to === undefined ? {} : { to }),
    sort: params.get("sort") ?? "start_time",
    order,
    limit,
    offset,
    includeEval: flag(params.get("include_eval")),
    includeExperiments: flag(params.get("include_experiments")),
    ...(params.get("search") ? { search: params.get("search") ?? "" } : {}),
    ...(params.get("filter") ? { filter: params.get("filter") ?? "" } : {}),
  };
}

function filterTraces(
  traces: readonly Schema<"TraceSummary">[],
  query: Query,
): Schema<"TraceSummary">[] {
  const userId = userIdOfFilter(query.filter);
  return visibleTraces(traces, query.includeEval, query.includeExperiments).filter((trace) => {
    if (!inRange(trace.start_time, query.from, query.to)) {
      return false;
    }
    if (userId !== undefined && trace.user_id !== userId) {
      return false;
    }
    if (!query.search) {
      return true;
    }
    const needle = query.search.toLowerCase();
    return [trace.name, trace.input_preview, trace.output_preview].some((value) =>
      value?.toLowerCase().includes(needle),
    );
  });
}

function filterSpans(
  spans: readonly Schema<"SpanSummary">[],
  query: Query,
): Schema<"SpanSummary">[] {
  return spans.filter((span) => {
    if (!query.includeEval && span.environment === "cetra-eval") {
      return false;
    }
    if (!query.includeExperiments && span.metadata["cetra.experiment_id"] !== undefined) {
      return false;
    }
    if (!inRange(span.start_time, query.from, query.to)) {
      return false;
    }
    if (!query.search) {
      return true;
    }
    const needle = query.search.toLowerCase();
    return [span.name, span.input_preview, span.output_preview].some((value) =>
      value?.toLowerCase().includes(needle),
    );
  });
}

function pageSpans(
  dataset: Dataset,
  traces: readonly Schema<"TraceSummary">[],
  query: Query,
): Response {
  const data: Schema<"SpanSummary">[] = [];
  let index = 0;
  let hasMore = false;
  for (const trace of traces) {
    for (const span of filterSpans(spansOf(dataset, trace.trace_id, query.order), query)) {
      if (index >= query.offset + query.limit) {
        hasMore = true;
        break;
      }
      if (index >= query.offset) {
        data.push(span);
      }
      index += 1;
    }
    if (hasMore) {
      break;
    }
  }
  return HttpResponse.json(
    { data, next_cursor: hasMore ? encodeCursor(query.offset + query.limit) : null },
    { headers: userIdOfFilter(query.filter) === undefined ? ignoredFilter(query) : {} },
  );
}

function spansOf(dataset: Dataset, traceId: string, order: Order): Schema<"SpanSummary">[] {
  const spans = (dataset.traceDetail(traceId)?.spans ?? [])
    .map((row) => row.span)
    .sort((left, right) => left.start_time.localeCompare(right.start_time));
  return order === "desc" ? spans.reverse() : spans;
}

function page<T>(rows: readonly T[], query: Query, headers: HeadersInit): Response {
  const data = rows.slice(query.offset, query.offset + query.limit);
  const next =
    query.offset + query.limit < rows.length ? encodeCursor(query.offset + query.limit) : null;
  return HttpResponse.json({ data, next_cursor: next }, { headers });
}

function sortTraces(
  traces: readonly Schema<"TraceSummary">[],
  sort: string,
  order: Order,
): Schema<"TraceSummary">[] {
  const direction = order === "asc" ? 1 : -1;
  return [...traces].sort((left, right) => {
    const diff = traceKey(left, sort) - traceKey(right, sort);
    return diff === 0 ? left.trace_id.localeCompare(right.trace_id) * direction : diff * direction;
  });
}

function sortSpans(
  spans: readonly Schema<"SpanSummary">[],
  sort: string,
  order: Order,
): Schema<"SpanSummary">[] {
  const direction = order === "asc" ? 1 : -1;
  return [...spans].sort((left, right) => (spanKey(left, sort) - spanKey(right, sort)) * direction);
}

function traceKey(trace: Schema<"TraceSummary">, sort: string): number {
  if (sort === "duration") {
    return trace.duration_ns;
  }
  if (sort === "cost_total") {
    return trace.cost_total ?? 0;
  }
  if (sort === "total_tokens") {
    return trace.total_tokens ?? 0;
  }
  return Date.parse(trace.start_time);
}

function spanKey(span: Schema<"SpanSummary">, sort: string): number {
  if (sort === "duration") {
    return span.duration_ns;
  }
  if (sort === "cost_total") {
    return span.cost_total ?? 0;
  }
  if (sort === "total_tokens") {
    return span.total_tokens ?? 0;
  }
  return Date.parse(span.start_time);
}

function compareSessions(
  left: Schema<"SessionSummary">,
  right: Schema<"SessionSummary">,
  sort: string,
  order: Order,
): number {
  const direction = order === "asc" ? 1 : -1;
  const key = (session: Schema<"SessionSummary">) => {
    if (sort === "end_time") {
      return Date.parse(session.end_time);
    }
    if (sort === "trace_count") {
      return session.trace_count;
    }
    if (sort === "cost_total") {
      return session.cost_total ?? 0;
    }
    return Date.parse(session.start_time);
  };
  return (key(left) - key(right)) * direction;
}

/** Nulls sort last in both directions; ties fall back to user_id in the same direction. */
export function compareUsers(
  left: UserGroup["user"],
  right: UserGroup["user"],
  sort: string,
  order: Order,
): number {
  const direction = order === "asc" ? 1 : -1;
  const leftKey = userSortKey(left, sort);
  const rightKey = userSortKey(right, sort);
  if (leftKey === null || rightKey === null) {
    if (leftKey !== rightKey) {
      return leftKey === null ? 1 : -1;
    }
  } else if (leftKey !== rightKey) {
    return (leftKey - rightKey) * direction;
  }
  const byId = left.user_id < right.user_id ? -1 : left.user_id > right.user_id ? 1 : 0;
  return byId * direction;
}

function userSortKey(user: UserGroup["user"], sort: string): number | null {
  switch (sort) {
    case "first_seen":
      return Date.parse(user.first_seen);
    case "trace_count":
      return user.trace_count;
    case "session_count":
      return user.session_count;
    case "traces_per_session":
      return user.session_count > 0 ? user.trace_count / user.session_count : null;
    case "total_tokens":
      return user.total_tokens ?? null;
    case "cost_total":
      return user.cost_total ?? null;
    case "error_count":
      return user.error_count;
    case "error_rate":
      return user.trace_count > 0 ? user.error_trace_count / user.trace_count : null;
    default:
      return Date.parse(user.last_seen);
  }
}

function inRange(start: string, from: number | undefined, to: number | undefined): boolean {
  const time = Date.parse(start);
  if (from !== undefined && time < from) {
    return false;
  }
  if (to !== undefined && time >= to) {
    return false;
  }
  return true;
}

function readLimit(value: string | null): number | Response {
  if (value === null) {
    return 50;
  }
  if (!/^\d+$/.test(value)) {
    return problem(400, "bad_request", "Bad Request");
  }
  const limit = Number(value);
  if (limit < 1 || limit > 1000) {
    return problem(400, "bad_request", "Bad Request");
  }
  return limit;
}

function readCursor(value: string | null): number | Response {
  if (value === null) {
    return 0;
  }
  try {
    const text = atob(value);
    if (!/^\d+$/.test(text)) {
      return problem(400, "bad_request", "Bad Request");
    }
    return Number(text);
  } catch {
    return problem(400, "bad_request", "Bad Request");
  }
}

function readTime(value: string | null): number | undefined | Response {
  if (value === null) {
    return undefined;
  }
  if (/^\d+$/.test(value)) {
    return Number(value);
  }
  const parsed = Date.parse(value);
  if (Number.isNaN(parsed)) {
    return problem(400, "bad_request", "Bad Request");
  }
  return parsed;
}

function encodeCursor(offset: number): string {
  return btoa(String(offset));
}

function flag(value: string | null): boolean {
  return value === "true" || value === "1";
}

function bucketOf(value: string | null): Schema<"MetricsBucket"> | undefined {
  const allowed = ["auto", "1m", "5m", "10m", "15m", "30m", "1h", "6h", "12h", "1d"];
  if (value !== null && allowed.includes(value)) {
    return value as Schema<"MetricsBucket">;
  }
  return undefined;
}

function ignoredFilter(query: Query): HeadersInit {
  if (!query.filter) {
    return {};
  }
  return { "X-Mock-Filter-Ignored": "1" };
}

function invalidFilter(): Response {
  return problem(400, "invalid_filter", "Bad Request", [
    { field: "filter", message: "unknown column bad", position: { line: 1, column: 1 } },
  ]);
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

function text(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

/** A trace is recent when it ended within the last 10 minutes. */
const RECENT_MS = 10 * 60 * 1000;
const PING_INTERVAL_MS = 15_000;

const extras = new WeakMap<Dataset, Map<string, Schema<"TraceDetail">>>();

type TraceLoop = {
  listeners: Set<StreamEmit>;
  timer: ReturnType<typeof setTimeout> | undefined;
  seq: number;
};

const traceLoops = new WeakMap<Dataset, TraceLoop>();

type SpanJob = {
  listeners: Set<StreamEmit>;
  timers: ReturnType<typeof setTimeout>[];
};

const spanJobs = new WeakMap<Dataset, Map<string, SpanJob>>();

let nextLiveSpan = 0;

export function liveTraceSource(dataset: Dataset): TraceStreamSource {
  return (emit) => {
    let loop = traceLoops.get(dataset);
    if (!loop) {
      loop = { listeners: new Set(), timer: undefined, seq: 1 };
      traceLoops.set(dataset, loop);
      scheduleCreate(dataset, loop);
    }
    loop.listeners.add(emit);
    return () => {
      const current = traceLoops.get(dataset);
      if (!current) {
        return;
      }
      current.listeners.delete(emit);
      if (current.listeners.size === 0) {
        if (current.timer !== undefined) {
          clearTimeout(current.timer);
        }
        traceLoops.delete(dataset);
      }
    };
  };
}

export function liveSpanSource(dataset: Dataset): SpanStreamSource {
  return (emit, traceId) => {
    const ping = setInterval(() => {
      emit.ping();
    }, PING_INTERVAL_MS);
    const stopSpans = watchSpans(dataset, traceId, emit);
    return () => {
      clearInterval(ping);
      stopSpans();
    };
  };
}

function scheduleCreate(dataset: Dataset, loop: TraceLoop): void {
  loop.timer = setTimeout(() => {
    if (traceLoops.get(dataset) !== loop) {
      return;
    }
    const created = addLiveTrace(dataset, loop.seq, Date.now());
    loop.seq += 1;
    emitTrace(loop, created);
    loop.timer = setTimeout(() => {
      if (traceLoops.get(dataset) !== loop) {
        return;
      }
      const updated = updateLiveTrace(dataset, created.trace_id, Date.now());
      if (updated) {
        emitTrace(loop, updated);
      }
      scheduleCreate(dataset, loop);
    }, 1_000);
  }, 2_000);
}

function emitTrace(loop: TraceLoop, trace: Schema<"TraceSummary">): void {
  for (const listener of loop.listeners) {
    listener.trace(trace);
  }
}

function watchSpans(dataset: Dataset, traceId: string, emit: StreamEmit): () => void {
  const trace = dataset.traces.find((row) => row.trace_id === traceId);
  if (!trace?.root_span_id || !isRecent(trace)) {
    return () => {};
  }
  let jobs = spanJobs.get(dataset);
  if (!jobs) {
    jobs = new Map();
    spanJobs.set(dataset, jobs);
  }
  let job = jobs.get(traceId);
  if (!job) {
    job = { listeners: new Set(), timers: [] };
    jobs.set(traceId, job);
    for (let index = 1; index <= 3; index += 1) {
      const timer = setTimeout(() => {
        const current = spanJobs.get(dataset)?.get(traceId);
        if (!current) {
          return;
        }
        const span = addChildSpan(dataset, trace, Date.now());
        for (const listener of current.listeners) {
          listener.span(span);
        }
      }, index * 1_000);
      job.timers.push(timer);
    }
  }
  job.listeners.add(emit);
  return () => {
    const current = spanJobs.get(dataset)?.get(traceId);
    if (!current) {
      return;
    }
    current.listeners.delete(emit);
    if (current.listeners.size === 0) {
      for (const timer of current.timers) {
        clearTimeout(timer);
      }
      spanJobs.get(dataset)?.delete(traceId);
    }
  };
}

function addLiveTrace(dataset: Dataset, seq: number, now: number): Schema<"TraceSummary"> {
  const template = dataset.traces[0];
  const start = rfc3339(now);
  const rootId = liveSpanId();
  const trace: Schema<"TraceSummary"> = {
    trace_id: `c${seq.toString(16).padStart(31, "0")}`,
    root_span_id: rootId,
    has_root: true,
    name: "live-trace",
    start_time: start,
    end_time: start,
    duration_ns: 0,
    status: "ok",
    span_count: 1,
    error_count: 0,
    llm_count: 0,
    metadata: {},
    models: [],
    tags: [],
    scores: [],
    input_preview: "Live trace",
    output_preview: "In progress",
    ...(template?.environment ? { environment: template.environment } : {}),
    ...(template?.release ? { release: template.release } : {}),
  };
  dataset.traces.unshift(trace);
  remember(dataset, {
    trace,
    spans: [{ depth: 0, detached: false, span: rootSpan(trace, now) }],
    truncated: false,
  });
  return trace;
}

function updateLiveTrace(
  dataset: Dataset,
  traceId: string,
  now: number,
): Schema<"TraceSummary"> | undefined {
  const trace = dataset.traces.find((row) => row.trace_id === traceId);
  if (!trace) {
    return undefined;
  }
  const startMs = Date.parse(trace.start_time);
  trace.end_time = rfc3339(now);
  trace.duration_ns = Number.isFinite(startMs) ? Math.max(0, now - startMs) * 1_000_000 : 0;
  trace.span_count += 1;
  return trace;
}

const LIVE_SPAN_MS = 150;

function addChildSpan(
  dataset: Dataset,
  trace: Schema<"TraceSummary">,
  now: number,
): Schema<"SpanSummary"> {
  const detail = extraDetail(dataset, trace.trace_id) ?? dataset.traceDetail(trace.trace_id);
  const window = liveSpanWindow(detail, now);
  const span: Schema<"SpanSummary"> = {
    span_id: liveSpanId(),
    trace_id: trace.trace_id,
    ...(trace.root_span_id ? { parent_span_id: trace.root_span_id } : {}),
    span_kind: "span",
    name: "live span",
    start_time: rfc3339(window.start),
    end_time: rfc3339(window.start + window.duration),
    duration_ns: window.duration * 1_000_000,
    status: "ok",
    metadata: {},
    tags: [],
  };
  if (detail) {
    detail.spans.push({ depth: 1, detached: false, span });
    detail.trace.span_count += 1;
  }
  return span;
}

/**
 * Where a live span lands: inside the root span's window, so the timeline axis keeps its extent.
 * A trace whose root has no extent yet (one created live) grows from now.
 */
function liveSpanWindow(
  detail: Schema<"TraceDetail"> | undefined,
  now: number,
): { start: number; duration: number } {
  const root = detail?.spans.find((row) => row.span.parent_span_id === undefined)?.span;
  const rootStart = root ? Date.parse(root.start_time) : Number.NaN;
  const rootMs = root ? root.duration_ns / 1_000_000 : 0;
  if (!Number.isFinite(rootStart) || rootMs <= 0) {
    return { start: now, duration: 0 };
  }
  const duration = Math.min(LIVE_SPAN_MS, rootMs);
  const live = detail?.spans.filter((row) => row.span.name === "live span").length ?? 0;
  const fraction = (live * 0.381966) % 1;
  return { start: Math.floor(rootStart + (rootMs - duration) * fraction), duration };
}

function rootSpan(trace: Schema<"TraceSummary">, now: number): Schema<"SpanSummary"> {
  const time = rfc3339(now);
  return {
    span_id: trace.root_span_id ?? liveSpanId(),
    trace_id: trace.trace_id,
    span_kind: "agent",
    name: trace.name,
    start_time: time,
    end_time: time,
    duration_ns: 0,
    status: "ok",
    metadata: {},
    tags: [],
  };
}

function isRecent(trace: Schema<"TraceSummary">): boolean {
  const end = Date.parse(trace.end_time);
  return Number.isFinite(end) && Date.now() - end <= RECENT_MS;
}

function extraDetail(dataset: Dataset, traceId: string): Schema<"TraceDetail"> | undefined {
  return extras.get(dataset)?.get(traceId);
}

function remember(dataset: Dataset, detail: Schema<"TraceDetail">): void {
  let map = extras.get(dataset);
  if (!map) {
    map = new Map();
    extras.set(dataset, map);
  }
  map.set(detail.trace.trace_id, detail);
}

function spanDetailFrom(span: Schema<"SpanSummary">): Schema<"SpanDetail"> {
  return {
    ...span,
    attributes: {},
    conventions: [],
    events: [],
    ingest_time: span.end_time,
    links: [],
    media_refs: [],
    resource_attributes: {},
    schema_version: 1,
    truncated: [],
    scope_name: "cetra",
  };
}

function liveSpanId(): string {
  nextLiveSpan += 1;
  return `e${nextLiveSpan.toString(16).padStart(15, "0")}`;
}

function rfc3339(ms: number): string {
  return new Date(ms)
    .toISOString()
    .replace(/\.(\d{3})Z$/, (_match, fraction: string) => `.${fraction}000000Z`);
}
