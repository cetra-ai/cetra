import { type HttpHandler, HttpResponse, http } from "msw";
import { beforeAll, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset, rfc3339, visibleTraces } from "@/mocks/generated/dataset";
import { CODING_STORY_PROFILE } from "@/mocks/generated/story-coding";
import { storyChannelHandlers, storyEvaluateHandlers } from "@/mocks/generated/story-evaluate";
import { EXTRACTION_STORY_PROFILE } from "@/mocks/generated/story-extraction";
import { SUPPORT_STORY_PROFILE } from "@/mocks/generated/story-support";
import { VOICE_STORY_PROFILE } from "@/mocks/generated/story-voice";
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
import channelsFixture from "../../../../fixtures/api/static/channels.json";
import evaluatorsFixture from "../../../../fixtures/api/static/evaluators.json";
import onlineRulesFixture from "../../../../fixtures/api/static/online-rules.json";

/** The screenshot clock (`website/scripts/screenshots.mjs`). */
const NOW = Date.parse("2026-11-19T12:00:00Z");
const HOUR_MS = 60 * 60_000;
const DAY_MS = 24 * HOUR_MS;
const TRACES: Record<StoryId, number> = {
  support: 5_000,
  coding: 2_000,
  voice: 2_000,
  extraction: 2_000,
};
const PROFILES = {
  support: SUPPORT_STORY_PROFILE,
  coding: CODING_STORY_PROFILE,
  voice: VOICE_STORY_PROFILE,
  extraction: EXTRACTION_STORY_PROFILE,
};
const STORY_IDS: readonly StoryId[] = ["support", "coding", "voice", "extraction"];
const TESTS_EVALUATOR = "01900000-0000-7000-8000-000000570901";
const TESTS_RULE = "01900000-0000-7000-8000-000000570911";
const READBACK_EVALUATOR = "01900000-0000-7000-8000-000000570902";
const ON_CALL_PAGER = "01900000-0000-7000-8000-000000000503";
const INVOICE_V3 = "01900000-0000-7000-8000-000000570931";
const INVOICE_V4 = "01900000-0000-7000-8000-000000570932";
const INVOICE_ITEMS = 300;
const READBACK_REASON =
  "The agent offered 9:30 on Friday but never read the booking back before ending the call.";

let stories: Record<StoryId, Dataset>;

beforeAll(() => {
  stories = Object.fromEntries(
    STORY_IDS.map((story) => [
      story,
      createDataset({ seed: 1, now: NOW, traces: TRACES[story], profile: PROFILES[story] }),
    ]),
  ) as Record<StoryId, Dataset>;
});

/** The API base of a story's project: `/api/v1/projects/<encoded ref>`. */
function baseOf(story: StoryId): string {
  return `${window.location.origin}/api/v1/projects/${encodeURIComponent(storyProject(story).ref)}`;
}

/** Every story project's handlers, each under its own project. */
function storyHandlersOfEveryStory(): HttpHandler[] {
  return STORY_IDS.flatMap((story) => {
    const ref = storyProject(story).ref;
    return [...storyEvaluateHandlers(stories[story], ref), ...storyChannelHandlers(ref)];
  });
}

function serveEveryStory(): void {
  server.use(...storyHandlersOfEveryStory());
}

/** A catch-all that answers 404: what the story handlers leave alone is then a 404, not a network error. */
const NOT_FOUND = http.all("*", () => new HttpResponse(null, { status: 404 }));

/** Every story project's handlers, then the catch-all: a fall-through shows as its exact 404. */
function serveEveryStoryAndFallThrough(): void {
  server.use(...storyHandlersOfEveryStory(), NOT_FOUND);
}

/** The ids of a list response's objects. */
async function idsAt(path: string): Promise<string[]> {
  const body = (await (await fetch(path)).json()) as { data: { id: string }[] };
  return body.data.map((object) => object.id);
}

it("lists each story's evaluators and rules under its own project only", async () => {
  serveEveryStory();

  expect(await idsAt(`${baseOf("coding")}/evaluators`)).toEqual([TESTS_EVALUATOR]);
  expect(await idsAt(`${baseOf("voice")}/evaluators`)).toEqual([READBACK_EVALUATOR]);
  expect(await idsAt(`${baseOf("extraction")}/evaluators`)).toEqual([]);
  expect(await idsAt(`${baseOf("support")}/evaluators`)).toEqual(
    evaluatorsFixture.data.map((evaluator) => evaluator.id),
  );

  expect(await idsAt(`${baseOf("coding")}/online-rules`)).toEqual([TESTS_RULE]);
  expect(await idsAt(`${baseOf("voice")}/online-rules`)).toEqual([]);
  expect(await idsAt(`${baseOf("support")}/online-rules`)).toEqual(
    onlineRulesFixture.data.map((rule) => rule.id),
  );
});

it("answers a story object only under its own project, and lets other ids fall through", async () => {
  serveEveryStoryAndFallThrough();

  const coding = (await (
    await fetch(`${baseOf("coding")}/evaluators/${TESTS_EVALUATOR}`)
  ).json()) as Schema<"Evaluator">;
  expect(coding.kind).toBe("check");
  expect(coding.config).toEqual({ check: "regex", pattern: "\\d+ passed" });
  expect(coding.output_score).toEqual({ name: "tests_pass", data_type: "boolean" });

  const judgeFromCoding = await fetch(`${baseOf("coding")}/evaluators/${READBACK_EVALUATOR}`);
  expect(judgeFromCoding.status).toBe(404);
  const ruleFromVoice = await fetch(`${baseOf("voice")}/online-rules/${TESTS_RULE}`);
  expect(ruleFromVoice.status).toBe(404);
});

it("serves the coding rule and its stats, with each hour's pass rate from the dataset", async () => {
  serveEveryStory();
  const dataset = stories.coding;

  const rule = (await (
    await fetch(`${baseOf("coding")}/online-rules/${TESTS_RULE}`)
  ).json()) as Schema<"OnlineRule">;
  expect(rule.evaluator_id).toBe(TESTS_EVALUATOR);
  expect(rule.sample_rate).toBe(1);
  expect(rule.filter).toBeUndefined();

  const stats = (await (
    await fetch(`${baseOf("coding")}/online-rules/stats`)
  ).json()) as Schema<"OnlineRuleStatsResponse">;
  expect(stats.window).toEqual({ from: rfc3339(NOW - DAY_MS), to: rfc3339(NOW) });
  const [entry] = stats.data;
  expect(stats.data.map((item) => item.rule_id)).toEqual([TESTS_RULE]);

  // Each hour of the last 24, oldest first: the visible coding runs that started in it.
  const visible = visibleTraces(dataset.traces, false, false);
  const hours = Array.from({ length: 24 }, (_unused, index) => {
    const start = NOW - (24 - index) * HOUR_MS;
    return visible
      .filter((trace) => {
        const started = Date.parse(trace.start_time);
        return started >= start && started < start + HOUR_MS;
      })
      .flatMap((trace) => {
        const value = trace.scores.find((score) => score.name === "tests_pass")?.value;
        return value?.data_type === "boolean" ? [value.value] : [];
      });
  });
  const runs = hours.flat();
  expect(runs.length).toBeGreaterThan(0);

  expect(entry?.jobs_hourly.map((hour) => hour.done)).toEqual(hours.map((values) => values.length));
  expect(entry?.jobs_hourly.every((hour) => hour.failed === 0 && hour.skipped === 0)).toBe(true);
  expect(entry?.scores.hourly.map((hour) => hour.time)).toEqual(
    entry?.jobs_hourly.map((hour) => hour.time),
  );
  expect(entry?.scores.hourly.map((hour) => hour.value)).toEqual(
    hours.map((values) =>
      values.length === 0 ? undefined : values.filter((value) => value).length / values.length,
    ),
  );
  expect(entry?.scores.count).toBe(runs.length);
  expect(entry?.scores.pass_rate).toBe(runs.filter((value) => value).length / runs.length);
  expect(entry?.scores.data_type).toBe("boolean");

  expect(rule.jobs_last_24h.done).toBe(runs.length);
});

it("answers the judge's test with the reason and the rendered prompt", async () => {
  serveEveryStory();

  const result = (await (
    await fetch(`${baseOf("voice")}/evaluators/test`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        kind: "llm_judge",
        output_score: { name: "readback", data_type: "boolean" },
      }),
    })
  ).json()) as Schema<"EvaluatorTestResult">;
  expect(result.ok).toBe(true);
  expect(result.value).toEqual({ data_type: "boolean", value: false });
  expect(result.reason).toBe(READBACK_REASON);
  expect(result.variables).toEqual({
    input: "Hi, can I move my appointment to Friday morning?",
    output: "Friday at 9:30 is free. Should I book it?",
  });
  expect(result.prompt).toContain(
    "Caller: Hi, can I move my appointment to Friday morning?\nAgent: Friday at 9:30 is free. Should I book it?\n",
  );
  expect(result.prompt).not.toContain("{{");
  expect(result.judge_trace_id).toBeUndefined();
});

it("adds the on-call pager to the voice desk's channels and to no other story's", async () => {
  serveEveryStory();

  const voice = (await (
    await fetch(`${baseOf("voice")}/channels`)
  ).json()) as Schema<"ChannelPage">;
  expect(voice.data.map((channel) => channel.id)).toEqual([
    ...channelsFixture.data.map((channel) => channel.id),
    ON_CALL_PAGER,
  ]);
  const pager = voice.data.find((channel) => channel.id === ON_CALL_PAGER);
  expect(pager).toEqual({
    id: ON_CALL_PAGER,
    name: "On-call pager",
    kind: "webhook",
    target_hint: "hooks.example.com",
    available: true,
    recipients: [],
    header_names: ["X-Team"],
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  });
  expect(pager).not.toHaveProperty("last_delivery");

  expect(await idsAt(`${baseOf("coding")}/channels`)).not.toContain(ON_CALL_PAGER);
});

/** One score of an experiment's summary, by name. */
function scoreOf(summary: Schema<"ExperimentSummary"> | undefined, name: string) {
  return summary?.scores.find((score) => score.name === name);
}

it("compares the two invoice prompts: each field's pass rate, the cost and the item count", async () => {
  serveEveryStory();

  const body = (await (
    await fetch(
      `${baseOf("extraction")}/experiments/compare?ids=${INVOICE_V3},${INVOICE_V4}&limit=1`,
    )
  ).json()) as Schema<"ExperimentComparison">;
  const [v3, v4] = body.experiments;
  expect(v3?.experiment.id).toBe(INVOICE_V3);
  expect(v4?.experiment.id).toBe(INVOICE_V4);

  // Pass rates: 291 and 297 of 300 for total, 273 and 288 for the VAT number, all for the rest.
  const percent = (summary: Schema<"ExperimentSummary"> | undefined, name: string) =>
    Math.round((scoreOf(summary, name)?.pass_rate ?? 0) * 100);
  expect(percent(v3, "total")).toBe(97);
  expect(percent(v4, "total")).toBe(99);
  expect(percent(v3, "vat_number")).toBe(91);
  expect(percent(v4, "vat_number")).toBe(96);
  for (const summary of [v3, v4]) {
    expect(percent(summary, "vendor")).toBe(100);
    expect(percent(summary, "date")).toBe(100);
    expect(scoreOf(summary, "total")?.count).toBe(INVOICE_ITEMS);
  }
  expect(v3?.cost_total).toBeCloseTo(0.63, 6);
  expect(v4?.cost_total).toBeCloseTo(0.72, 6);
  expect(v3?.result_count).toBe(INVOICE_ITEMS);

  expect(body.deltas[0]?.scores.total?.delta).toBeCloseTo(6 / INVOICE_ITEMS, 9);
  expect(body.rows).toHaveLength(1);
  expect(body.next_cursor).not.toBeNull();
});

it("pages the results of an invoice run over all 300 items", async () => {
  serveEveryStory();

  const seen: string[] = [];
  let cursor: string | null = null;
  do {
    const query = cursor === null ? "" : `&cursor=${encodeURIComponent(cursor)}`;
    const page = (await (
      await fetch(`${baseOf("extraction")}/experiments/${INVOICE_V3}/results?limit=120${query}`)
    ).json()) as { rows: Schema<"ResultRow">[]; next_cursor: string | null };
    seen.push(...page.rows.map((row) => row.dataset_item_id));
    cursor = page.next_cursor;
  } while (cursor !== null);
  expect(seen).toHaveLength(INVOICE_ITEMS);
  expect(new Set(seen).size).toBe(INVOICE_ITEMS);
});

it("serves the invoice experiments under the extraction project only", async () => {
  serveEveryStoryAndFallThrough();

  expect(await idsAt(`${baseOf("extraction")}/experiments`)).toEqual([INVOICE_V4, INVOICE_V3]);
  const support = await fetch(`${baseOf("support")}/experiments/${INVOICE_V3}`);
  expect(support.status).toBe(404);
  const coding = await fetch(
    `${baseOf("coding")}/experiments/compare?ids=${INVOICE_V3},${INVOICE_V4}`,
  );
  expect(coding.status).toBe(404);
});

it("answers the story objects without a project: both evaluators and the invoice runs by id", async () => {
  server.use(...storyEvaluateHandlers(stories.support), NOT_FOUND);
  const unscoped = `${window.location.origin}/api/v1/projects/acme%2Fany`;

  expect(await idsAt(`${unscoped}/evaluators`)).toEqual(
    expect.arrayContaining([TESTS_EVALUATOR, READBACK_EVALUATOR]),
  );
  const run = await fetch(`${unscoped}/experiments/${INVOICE_V3}`);
  expect(run.status).toBe(200);
  expect(((await run.json()) as Schema<"Experiment">).id).toBe(INVOICE_V3);
  const compare = await fetch(
    `${unscoped}/experiments/compare?ids=${INVOICE_V3},${INVOICE_V4}&limit=1`,
  );
  expect(compare.status).toBe(200);
  // The list is the generated one, not the invoice list: with no project it falls through.
  expect((await fetch(`${unscoped}/experiments`)).status).toBe(404);
});
