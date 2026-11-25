import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset, type Dataset, EXTRACTION_SHOWCASE, type Profile } from "./dataset";
import { CODING_PROFILE, EXTRACTION_PROFILE, VOICE_PROFILE } from "./profiles";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const SEED = 1;
const TRACES = 2000;
const DAY_MS = 86_400_000;

type TraceSummary = Schema<"TraceSummary">;

/** A trace's dataset index: its id is `a` and the index in hex. */
function indexOf(trace: TraceSummary): number {
  return Number.parseInt(trace.trace_id.slice(1), 16);
}

function build(profile: Profile): Dataset {
  return createDataset({ seed: SEED, now: NOW, traces: TRACES, profile });
}

function byIndex(dataset: Dataset, index: number): TraceSummary | undefined {
  return dataset.traces.find((trace) => indexOf(trace) === index);
}

/** The generated traces (not the profile's showcases), in index order. */
function generated(dataset: Dataset, profile: Profile): TraceSummary[] {
  const heroes = new Set(profile.showcases.map((showcase) => showcase.index));
  return dataset.traces
    .filter((trace) => !heroes.has(indexOf(trace)))
    .sort((a, b) => indexOf(a) - indexOf(b));
}

function durationMs(trace: TraceSummary): number {
  return trace.duration_ns / 1_000_000;
}

/** Every question a profile's turns ask: the topics' and their follow-ups'. */
function questionsOf(profile: Profile): Set<string> {
  const questions = new Set<string>();
  for (const topic of profile.turns) {
    questions.add(topic.question);
    for (const followUp of topic.followUps) {
      questions.add(followUp.question);
    }
  }
  return questions;
}

it("each profile holds its own hero only", () => {
  const coding = build(CODING_PROFILE);
  const hero = byIndex(coding, 4);
  expect(hero?.name).toBe("coding-agent");
  expect(hero?.user_id).toBe("dev-007");
  const codingQuestions = questionsOf(CODING_PROFILE);
  for (const index of [3, 5, 6]) {
    const trace = byIndex(coding, index);
    expect(trace?.user_id).toMatch(/^dev-\d{3}$/);
    expect(codingQuestions.has(trace?.input_preview ?? "")).toBe(true);
  }

  const voice = build(VOICE_PROFILE);
  expect(byIndex(voice, 5)?.user_id).toBe("caller-019");
  const voiceQuestions = questionsOf(VOICE_PROFILE);
  for (const index of [3, 4, 6]) {
    const trace = byIndex(voice, index);
    expect(trace?.user_id).toMatch(/^caller-\d{3}$/);
    expect(voiceQuestions.has(trace?.input_preview ?? "")).toBe(true);
  }

  const extraction = build(EXTRACTION_PROFILE);
  const invoice = byIndex(extraction, 6);
  expect(EXTRACTION_SHOWCASE.story).toBeDefined();
  expect(invoice?.input_preview).toBe(EXTRACTION_SHOWCASE.story?.inputPreview);
  const extractionQuestions = questionsOf(EXTRACTION_PROFILE);
  for (const index of [3, 4, 5]) {
    const trace = byIndex(extraction, index);
    expect(trace?.user_id).toBeUndefined();
    expect(extractionQuestions.has(trace?.input_preview ?? "")).toBe(true);
  }
});

it("coding runs are long and tool-heavy", () => {
  const dataset = build(CODING_PROFILE);
  const traces = generated(dataset, CODING_PROFILE);
  for (const trace of traces) {
    expect(durationMs(trace)).toBeGreaterThanOrEqual(8_000);
    expect(durationMs(trace)).toBeLessThan(90_000);
    expect(trace.span_count).toBeGreaterThanOrEqual(20);
    expect(trace.span_count).toBeLessThan(201);
  }

  const toolNames = new Set(["read_file", "search_code", "apply_patch", "run_tests", "git_diff"]);
  const rows = dataset.traceDetail(byIndex(dataset, 1)?.trace_id ?? "")?.spans ?? [];
  const tools = rows.filter((row) => row.span.span_kind === "tool");
  expect(tools.length / rows.length).toBeGreaterThanOrEqual(0.4);
  for (const row of tools) {
    expect(toolNames.has(row.span.name)).toBe(true);
  }

  const errored = traces.filter((trace) => trace.status === "error");
  const rate = errored.length / traces.length;
  expect(rate).toBeGreaterThanOrEqual(0.08);
  expect(rate).toBeLessThanOrEqual(0.16);
  for (const trace of errored) {
    const errorRows = (dataset.traceDetail(trace.trace_id)?.spans ?? []).filter(
      (row) => row.span.status === "error",
    );
    expect(errorRows.length).toBeGreaterThan(0);
    for (const row of errorRows) {
      expect(row.span.status_message).toBe("1 failed, 47 passed");
    }
  }
});

it("coding users are developers", () => {
  const dataset = build(CODING_PROFILE);
  for (const trace of dataset.traces) {
    expect(trace.user_id).toMatch(/^dev-\d{3}$/);
  }
});

it("voice turns are short and in calls", () => {
  const dataset = build(VOICE_PROFILE);
  const traces = generated(dataset, VOICE_PROFILE);
  const positions = new Map(traces.map((trace, position) => [indexOf(trace), position]));

  const calls = new Map<string, TraceSummary[]>();
  for (const trace of traces) {
    expect(trace.tags).toContain("voice");
    expect(durationMs(trace)).toBeLessThan(4_000);
    expect(trace.span_count).toBeGreaterThanOrEqual(5);
    expect(trace.span_count).toBeLessThan(10);
    expect(trace.session_id).toMatch(/^call-\d{4}$/);
    expect(trace.user_id).toMatch(/^caller-\d{3}$/);
    const session = trace.session_id ?? "";
    calls.set(session, [...(calls.get(session) ?? []), trace]);
  }

  const lastPosition = traces.length - 1;
  for (const [session, members] of calls) {
    const first = positions.get(indexOf(members[0] as TraceSummary)) ?? -1;
    const last = positions.get(indexOf(members[members.length - 1] as TraceSummary)) ?? -1;
    // Consecutive among the generated traces: the showcase index is skipped, not broken across.
    expect(last - first + 1, session).toBe(members.length);
    for (const member of members) {
      expect(member.user_id, session).toBe(members[0]?.user_id);
    }
    const holdsLast = last === lastPosition;
    expect(members.length, session).toBeLessThanOrEqual(12);
    if (!holdsLast) {
      expect(members.length, session).toBeGreaterThanOrEqual(4);
    } else {
      expect(members.length, session).toBeGreaterThanOrEqual(1);
    }
  }

  const voiceQuestions = new Set(VOICE_PROFILE.turns.map((topic) => topic.question));
  // Experiment traces are hidden by `sessions(false, false)`, which can hide a session's first turn; include them.
  const sessions = dataset.sessions(true, true);
  expect(sessions.length).toBeGreaterThan(0);
  for (const session of sessions) {
    expect(voiceQuestions.has(session.first_input_preview ?? "")).toBe(true);
  }
});

it("voice calls keep their first turn's age", () => {
  const dataset = build(VOICE_PROFILE);
  const recent = dataset.traces.filter((trace) => NOW - Date.parse(trace.start_time) < DAY_MS);
  const share = recent.length / dataset.traces.length;
  expect(share).toBeGreaterThanOrEqual(0.4);
  expect(share).toBeLessThanOrEqual(0.7);

  const calls = new Map<string, TraceSummary[]>();
  for (const trace of generated(dataset, VOICE_PROFILE)) {
    const session = trace.session_id ?? "";
    calls.set(session, [...(calls.get(session) ?? []), trace]);
  }
  for (const [session, members] of calls) {
    const starts = members.map((member) => Date.parse(member.start_time));
    for (let turn = 1; turn < starts.length; turn += 1) {
      expect(starts[turn], session).toBeGreaterThan(starts[turn - 1] ?? Number.NaN);
    }
  }
});

it("extraction is a batch", () => {
  const dataset = build(EXTRACTION_PROFILE);
  for (const trace of dataset.traces) {
    expect(trace.session_id).toBeUndefined();
    expect(trace.user_id).toBeUndefined();
    expect(trace.tags).toContain("billing");
    const score = trace.scores.find((entry) => entry.name === "schema_valid");
    expect(score?.value).toEqual({ data_type: "boolean", value: trace.status !== "error" });
    expect(trace.cost_total).toBeLessThan(0.02);
  }
});

it("profiles are deterministic", () => {
  for (const profile of [CODING_PROFILE, VOICE_PROFILE, EXTRACTION_PROFILE]) {
    expect(build(profile).traces).toEqual(build(profile).traces);
  }
});
