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
function serveEveryStory(): void {
  server.use(
    ...STORY_IDS.flatMap((story) => {
      const ref = storyProject(story).ref;
      return [...storyEvaluateHandlers(stories[story], ref), ...storyChannelHandlers(ref)];
    }),
  );
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
  serveEveryStory();

  const coding = (await (
    await fetch(`${baseOf("coding")}/evaluators/${TESTS_EVALUATOR}`)
  ).json()) as Schema<"Evaluator">;
  expect(coding.kind).toBe("check");
  expect(coding.config).toEqual({ check: "regex", pattern: "\\d+ passed" });
  expect(coding.output_score).toEqual({ name: "tests_pass", data_type: "boolean" });

  const judgeFromCoding = await fetch(`${baseOf("coding")}/evaluators/${READBACK_EVALUATOR}`);
  expect(judgeFromCoding.status).not.toBe(200);
  const ruleFromVoice = await fetch(`${baseOf("voice")}/online-rules/${TESTS_RULE}`);
  expect(ruleFromVoice.status).not.toBe(200);
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
