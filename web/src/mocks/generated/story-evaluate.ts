import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { type Dataset, rfc3339 } from "@/mocks/generated/dataset";
import { ON_CALL_PAGER, startedBetween } from "@/mocks/generated/stories";
import channelsFixture from "../../../../fixtures/api/static/channels.json";
import evaluatorsFixture from "../../../../fixtures/api/static/evaluators.json";
import onlineRuleStatsFixture from "../../../../fixtures/api/static/online-rule-stats.json";
import onlineRulesFixture from "../../../../fixtures/api/static/online-rules.json";
import { projectOfRef, type StoryId } from "../projects";

/**
 * The evaluation objects of the story projects (M18 P5): the on-call channel of the voice desk, the
 * coding agent's check and its online rule, and the voice desk's LLM judge with its test result.
 * Only the story projects answer them (`storyEvaluateHandlers`, `storyChannelHandlers`).
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

/** The values of the boolean score `name` of each trace that carries it. */
function booleanValues(traces: readonly Schema<"TraceSummary">[], name: string): boolean[] {
  return traces.flatMap((trace) => {
    const value = trace.scores.find((score) => score.name === name)?.value;
    return value?.data_type === "boolean" ? [value.value] : [];
  });
}

/** The hours before the dataset's now, oldest first: each hour's `tests_pass` values. */
function hoursOf(dataset: Dataset): Hour[] {
  return Array.from({ length: HOURS }, (_unused, index) => {
    const start = dataset.now - (HOURS - index) * HOUR_MS;
    return {
      start,
      values: booleanValues(startedBetween(dataset, start, start + HOUR_MS), "tests_pass"),
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

/** A path parameter as text; msw types it as possibly repeated or missing. */
function idParam(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
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
  ];
}
