import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, rfc3339 } from "@/mocks/generated/dataset";
import {
  compareBody,
  itemId,
  type Run,
  runMetadata,
  summaryOf,
} from "@/mocks/generated/experiments";
import { mulberry32 } from "@/mocks/generated/random";
import { idParam, ON_CALL_PAGER, scoreValues, startedBetween } from "@/mocks/generated/stories";
import channelsFixture from "../../../../fixtures/api/static/channels.json";
import evaluatorsFixture from "../../../../fixtures/api/static/evaluators.json";
import onlineRuleStatsFixture from "../../../../fixtures/api/static/online-rule-stats.json";
import onlineRulesFixture from "../../../../fixtures/api/static/online-rules.json";
import { projectOfRef, type StoryId } from "../projects";

/**
 * The evaluation objects of the story projects (M18 P5): the on-call channel of the voice desk, the
 * coding agent's check and its online rule, and the voice desk's LLM judge with its test result.
 * Only the story projects answer them (`storyEvaluateHandlers`, `storyChannelHandlers`). The
 * extraction project's two experiments on the labeled invoices (M18 P6) are answered there too.
 */

const PROJECT = "/api/v1/projects/:project";
const HOUR_MS = 60 * 60_000;
const DAY_MS = 24 * HOUR_MS;
const MEMBER_ID = "01900000-0000-7000-8000-000000000100";
const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const DEFINED_AT = "2026-01-01T09:00:00.000000000Z";

const TESTS_EVALUATOR_ID = "01900000-0000-7000-8000-000000570901";
const TESTS_RULE_ID = "01900000-0000-7000-8000-000000570911";
const READBACK_EVALUATOR_ID = "01900000-0000-7000-8000-000000570902";

/** The on-call pager of the voice desk: the fixture "Pager webhook" with a name of its own and no delivery yet. */
const ON_CALL_CHANNEL: Schema<"NotificationChannel"> = {
  id: ON_CALL_PAGER.id,
  name: ON_CALL_PAGER.name,
  kind: "webhook",
  target_hint: "hooks.example.com",
  available: true,
  recipients: [],
  header_names: ["X-Team"],
  created_at: DEFINED_AT,
  updated_at: DEFINED_AT,
};

/**
 * An evaluator as served. The generated type leaves `config` empty (`Record<string, never>`), so
 * the configuration is typed here.
 */
type EvaluatorObject = Omit<Schema<"Evaluator">, "config"> & { config: Record<string, unknown> };

/** The coding agent's check: the patch's test run prints "N passed". */
const TESTS_EVALUATOR: EvaluatorObject = {
  id: TESTS_EVALUATOR_ID,
  name: "Tests pass after the patch",
  kind: "check",
  config: { check: "regex", pattern: "\\d+ passed" },
  output_score: { name: "tests_pass", data_type: "boolean" },
  has_signing_secret: false,
  version: 1,
  created_by: MEMBER_ID,
  created_at: DEFINED_AT,
  updated_at: DEFINED_AT,
};

const READBACK_PROMPT =
  "Did the agent read the booking back to the caller in one sentence (day, time, doctor)? Caller: {{input}}\nAgent: {{output}}\nAnswer true or false, then why.";

/** The voice desk's LLM judge: did the agent read the booking back? */
const READBACK_EVALUATOR: EvaluatorObject = {
  id: READBACK_EVALUATOR_ID,
  name: "Reads the booking back",
  kind: "llm_judge",
  config: {
    connection_id: CONNECTION_ID,
    model: "gpt-4.1-mini",
    prompt: READBACK_PROMPT,
    output: { type: "boolean" },
    include_reason: true,
    temperature: 0,
    max_output_tokens: 200,
    timeout_ms: 20_000,
  },
  output_score: { name: "readback", data_type: "boolean" },
  has_signing_secret: false,
  version: 1,
  created_by: MEMBER_ID,
  created_at: DEFINED_AT,
  updated_at: DEFINED_AT,
};

/** The judge's test input: one caller turn and the agent's reply, which does not read the booking back. */
const READBACK_INPUT = "Hi, can I move my appointment to Friday morning?";
const READBACK_OUTPUT = "Friday at 9:30 is free. Should I book it?";

/** The hours of the stats: the 24 hours before now, oldest first. */
const HOURS = 24;

/** The coding runs of one hour: their `tests_pass` values. */
type Hour = { start: number; values: boolean[] };

/**
 * The hours before the dataset's now, oldest first: each hour's `tests_pass` values. The hours are
 * UTC hours, as the story rules' hourly buckets are (`firstDippedHourAgo` in stories.ts).
 */
function hoursOf(dataset: Dataset): Hour[] {
  const end = Math.floor(dataset.now / HOUR_MS) * HOUR_MS;
  return Array.from({ length: HOURS }, (_unused, index) => {
    const start = end - (HOURS - index) * HOUR_MS;
    return {
      start,
      values: scoreValues(startedBetween(dataset, start, start + HOUR_MS), "tests_pass", "boolean"),
    };
  });
}

/** The online rule's jobs are the coding runs that have a `tests_pass` score, over the 24 hours. */
function testsRule(dataset: Dataset): Schema<"OnlineRule"> {
  const done = hoursOf(dataset).reduce((total, hour) => total + hour.values.length, 0);
  return {
    id: TESTS_RULE_ID,
    name: "Tests pass on every run",
    evaluator_id: TESTS_EVALUATOR_ID,
    target: "trace",
    variable_mapping: { input: "trace.input", output: "trace.output" },
    sample_rate: 1,
    enabled: true,
    jobs_last_24h: { queued: 0, running: 0, done, failed: 0, skipped: 0 },
    dropped_last_hour: 0,
    created_by: MEMBER_ID,
    created_at: DEFINED_AT,
    updated_at: DEFINED_AT,
  };
}

/** The rule's stats of the 24 hours: per hour the jobs and the pass rate, then the totals. */
function testsRuleStats(dataset: Dataset): Schema<"OnlineRuleStats"> {
  const hours = hoursOf(dataset);
  const values = hours.flatMap((hour) => hour.values);
  const passes = values.filter((value) => value).length;
  return {
    rule_id: TESTS_RULE_ID,
    jobs_hourly: hours.map((hour) => ({
      time: rfc3339(hour.start),
      done: hour.values.length,
      failed: 0,
      skipped: 0,
    })),
    scores: {
      data_type: "boolean",
      count: values.length,
      hourly: hours.map((hour) => ({
        time: rfc3339(hour.start),
        ...(hour.values.length === 0
          ? {}
          : { value: hour.values.filter((value) => value).length / hour.values.length }),
      })),
      ...(values.length === 0 ? {} : { pass_rate: passes / values.length }),
    },
  };
}

/** The judge's test run on the booking question: the reason and the rendered prompt. */
function readbackTestResult(): Schema<"EvaluatorTestResult"> {
  const variables = { input: READBACK_INPUT, output: READBACK_OUTPUT };
  return {
    ok: true,
    value: { data_type: "boolean", value: false },
    reason:
      "The agent offered 9:30 on Friday but never read the booking back before ending the call.",
    prompt: READBACK_PROMPT.replaceAll("{{input}}", variables.input).replaceAll(
      "{{output}}",
      variables.output,
    ),
    variables,
    usage: { input_tokens: 412, output_tokens: 38, cost_usd: 0.00023 },
    latency_ms: 840,
    trace_truncated: false,
  };
}

/** The `rule_id` query parameter of a request, when it has one. */
function ruleIdOf(request: Request): string | null {
  return new URL(request.url).searchParams.get("rule_id");
}

/**
 * The on-call channel of the voice desk, next to the fixture channels. Without `project` every
 * project gets it; another story project and the fixtures' projects fall through.
 */
export function storyChannelHandlers(project?: string): HttpHandler[] {
  if (project !== undefined && projectOfRef(project)?.story !== "voice") {
    return [];
  }
  const prefix =
    project === undefined ? PROJECT : `/api/v1/projects/${encodeURIComponent(project)}`;
  return [
    http.get(`${prefix}/channels`, () =>
      HttpResponse.json({
        data: [...channelsFixture.data, ON_CALL_CHANNEL],
        next_cursor: null,
      }),
    ),
  ];
}

// The invoice experiment (M18 P6): two prompt versions run over the same 300 labeled invoices.

const INVOICE_DATASET_ID = "01900000-0000-7000-8000-000000570921";
const INVOICE_ITEMS = 300;
/**
 * The dataset offset `itemId` takes for the invoices. The support runs use 0 and 1. Offset 2's 300
 * rows would run past its 0x100 slot into offset 3's (rows 256 to 299), so the invoices take 4 and
 * offset 3 stays unused.
 */
const INVOICE_ITEM_OFFSET = 4;
const INVOICE_THRESHOLD = 0.1;
const INVOICE_LIMIT = 50;
const INVOICE_MAX_LIMIT = 200;

/**
 * The invoices a run passes. Each field's passes are the first rows of an order: `total` and
 * `vat_number` use two different orders (salts 1 and 2), so the rows that fail differ per field.
 */
type InvoiceRun = {
  id: string;
  version: number;
  createdAt: string;
  total: number;
  vatNumber: number;
  cost: number;
  latencyMs: number;
};

const INVOICE_RUNS: readonly InvoiceRun[] = [
  {
    id: "01900000-0000-7000-8000-000000570931",
    version: 3,
    createdAt: "2026-11-18T10:00:00.000Z",
    total: 291,
    vatNumber: 273,
    cost: 0.0021,
    latencyMs: 2_100,
  },
  {
    id: "01900000-0000-7000-8000-000000570932",
    version: 4,
    createdAt: "2026-11-18T11:00:00.000Z",
    total: 297,
    vatNumber: 288,
    cost: 0.0024,
    latencyMs: 2_300,
  },
];

/** The rank of each row (0 first) in the order of `salt`: the rows sorted by a hash of salt and row. */
function rankOf(salt: number): number[] {
  const keys = Array.from({ length: INVOICE_ITEMS }, (_unused, row) =>
    mulberry32(salt * 1_000_003 + row)(),
  );
  const order = keys
    .map((_key, row) => row)
    .sort((left, right) => (keys[left] ?? 0) - (keys[right] ?? 0));
  const rank = new Array<number>(INVOICE_ITEMS).fill(0);
  order.forEach((row, position) => {
    rank[row] = position;
  });
  return rank;
}

const TOTAL_RANK = rankOf(1);
const VAT_RANK = rankOf(2);

/** A boolean score of one row. */
function passed(pass: boolean): Schema<"ResultScore"> {
  return { data_type: "boolean", value_num: pass ? 1 : 0 };
}

/** The rows of one invoice run: the same 300 items in both runs, with the run's passes. */
function invoiceRowsOf(run: InvoiceRun): Schema<"ResultRow">[] {
  return Array.from({ length: INVOICE_ITEMS }, (_unused, row) => ({
    dataset_item_id: itemId(INVOICE_ITEM_OFFSET, row),
    repetition: 0,
    input: { invoice: `invoice-${String(row + 1).padStart(4, "0")}.pdf` },
    latency_ns: run.latencyMs * 1_000_000,
    cost_total: run.cost,
    scores: {
      total: passed((TOTAL_RANK[row] ?? 0) < run.total),
      vat_number: passed((VAT_RANK[row] ?? 0) < run.vatNumber),
      vendor: passed(true),
      date: passed(true),
    },
  }));
}

const INVOICE_BY_ID = new Map<string, Run>(
  INVOICE_RUNS.map((spec): [string, Run] => [
    spec.id,
    {
      experiment: {
        id: spec.id,
        name: `extract-invoice v${spec.version}`,
        dataset_id: INVOICE_DATASET_ID,
        dataset_name: "Labeled invoices",
        dataset_version: 1,
        metadata: runMetadata({
          prompt: "extract-invoice",
          version: spec.version,
          model: "gemini-2.5-flash",
        }),
        repetitions: 1,
        item_count: INVOICE_ITEMS,
        result_count: INVOICE_ITEMS,
        status: "completed",
        created_at: spec.createdAt,
        finished_at: new Date(Date.parse(spec.createdAt) + 5 * 60_000).toISOString(),
      },
      rows: invoiceRowsOf(spec),
      datasetKey: INVOICE_DATASET_ID,
    },
  ]),
);

/** The two runs, newest first, as the list returns them. */
const INVOICE_LIST: Run[] = [...INVOICE_BY_ID.values()].sort((left, right) =>
  right.experiment.created_at.localeCompare(left.experiment.created_at),
);

/** A positive integer query parameter, or the default; the limit's maximum applies. */
function limitOf(url: URL): number {
  const parsed = Number(url.searchParams.get("limit"));
  return Number.isInteger(parsed) && parsed > 0
    ? Math.min(parsed, INVOICE_MAX_LIMIT)
    : INVOICE_LIMIT;
}

/** The compare of two to four invoice runs, as the server's compare route answers it. */
function invoiceCompare(url: URL): Response | undefined {
  const runs = (url.searchParams.get("ids") ?? "").split(",").map((id) => INVOICE_BY_ID.get(id));
  if (runs.length < 2 || runs.length > 4 || runs.some((run) => run === undefined)) {
    return undefined;
  }
  const thresholdText = url.searchParams.get("threshold");
  const threshold = thresholdText === null ? INVOICE_THRESHOLD : Number(thresholdText);
  const onlyText = url.searchParams.get("only");
  const only =
    onlyText === "regressed" || onlyText === "improved" || onlyText === "changed"
      ? onlyText
      : undefined;
  return HttpResponse.json(
    compareBody(
      runs.filter((run): run is Run => run !== undefined),
      threshold,
      only,
      limitOf(url),
      url.searchParams.get("cursor") ?? undefined,
    ),
  );
}

/** The results of an invoice run, one page of items after the cursor (the item id of the last one). */
function invoiceResults(url: URL, id: string): Response | undefined {
  const run = INVOICE_BY_ID.get(id);
  if (!run) {
    return undefined;
  }
  const cursor = url.searchParams.get("cursor");
  const limit = limitOf(url);
  const rows = run.rows.filter((row) => cursor === null || row.dataset_item_id > cursor);
  const shown = rows.slice(0, limit);
  const last = shown[shown.length - 1];
  return HttpResponse.json({
    summary: summaryOf(run),
    rows: shown,
    next_cursor: rows.length > limit && last ? last.dataset_item_id : null,
  });
}

/**
 * The invoice experiments' routes, under `prefix`. The list is answered only with `scoped` (a
 * project ref), so an unscoped request keeps the generated experiments' list; the by-id routes
 * answer their own ids in either case and fall through for any other.
 */
function invoiceExperimentHandlers(prefix: string, scoped: boolean): HttpHandler[] {
  const EXPERIMENTS = `${prefix}/experiments`;
  return [
    ...(scoped
      ? [
          http.get(EXPERIMENTS, () =>
            HttpResponse.json({
              data: INVOICE_LIST.map((run) => run.experiment),
              next_cursor: null,
            }),
          ),
        ]
      : []),
    http.get(`${EXPERIMENTS}/compare`, ({ request }) => invoiceCompare(new URL(request.url))),
    http.get(`${EXPERIMENTS}/:id/results`, ({ request, params }) =>
      invoiceResults(new URL(request.url), idParam(params.id)),
    ),
    http.get(`${EXPERIMENTS}/:id`, ({ params }) => {
      const run = INVOICE_BY_ID.get(idParam(params.id));
      return run ? HttpResponse.json(run.experiment) : undefined;
    }),
  ];
}

/**
 * The evaluators, online rules and judge test of the story projects. With `project` (an API ref,
 * `acme/<slug>`) the routes use its literal prefix and serve only that project's story objects
 * (none for a ref that is no story project); without it, every story object is served. The
 * fixtures' evaluators, rules and stats are listed only under support, as in `storyHandlers`.
 * A request for an object the project does not own falls through to the fixtures.
 */
export function storyEvaluateHandlers(dataset: Dataset, project?: string): HttpHandler[] {
  const prefix =
    project === undefined ? PROJECT : `/api/v1/projects/${encodeURIComponent(project)}`;
  const scope = project === undefined ? undefined : projectOfRef(project)?.story;
  const served = (story: StoryId) => project === undefined || story === scope;
  const withFixtures = project === undefined || scope === "support";
  const coding = served("coding");
  const voice = served("voice");
  const extraction = served("extraction");
  const evaluators = [...(coding ? [TESTS_EVALUATOR] : []), ...(voice ? [READBACK_EVALUATOR] : [])];
  const EVALUATORS = `${prefix}/evaluators`;
  const RULES = `${prefix}/online-rules`;
  return [
    http.get(EVALUATORS, () =>
      HttpResponse.json({
        data: [...evaluators, ...(withFixtures ? evaluatorsFixture.data : [])],
        next_cursor: null,
      }),
    ),
    http.get(`${EVALUATORS}/:id`, ({ params }) => {
      const found = evaluators.find((evaluator) => evaluator.id === idParam(params.id));
      return found ? HttpResponse.json(found) : undefined;
    }),
    http.post(`${EVALUATORS}/test`, () =>
      voice ? HttpResponse.json(readbackTestResult()) : undefined,
    ),
    http.get(RULES, () => {
      const rules = coding ? [testsRule(dataset)] : [];
      return HttpResponse.json({
        data: [...rules, ...(withFixtures ? onlineRulesFixture.data : [])],
        org_enabled_rules: rules.length + (withFixtures ? onlineRulesFixture.org_enabled_rules : 0),
        next_cursor: null,
      });
    }),
    // Before `:id`, which would match "stats".
    http.get(`${RULES}/stats`, () =>
      HttpResponse.json({
        window: { from: rfc3339(dataset.now - DAY_MS), to: rfc3339(dataset.now) },
        data: [
          ...(coding ? [testsRuleStats(dataset)] : []),
          ...(withFixtures ? onlineRuleStatsFixture.data : []),
        ],
      }),
    ),
    http.get(`${RULES}/:id`, ({ params }) =>
      coding && idParam(params.id) === TESTS_RULE_ID
        ? HttpResponse.json(testsRule(dataset))
        : undefined,
    ),
    http.get(`${RULES}/:id/backfills`, ({ params }) =>
      coding && idParam(params.id) === TESTS_RULE_ID
        ? HttpResponse.json({ data: [], next_cursor: null })
        : undefined,
    ),
    // The rule drawer's lists of its own rule: none, so the fixtures' jobs and scores stay out.
    http.get(`${prefix}/eval-jobs`, ({ request }) =>
      coding && ruleIdOf(request) === TESTS_RULE_ID
        ? HttpResponse.json({ data: [], next_cursor: null })
        : undefined,
    ),
    http.get(`${prefix}/scores`, ({ request }) =>
      coding && ruleIdOf(request) === TESTS_RULE_ID
        ? HttpResponse.json({ data: [], next_cursor: null })
        : undefined,
    ),
    ...(extraction ? invoiceExperimentHandlers(prefix, project !== undefined) : []),
  ];
}
