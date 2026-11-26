import { describe, expect, it } from "vitest";
import { createDataset, type Dataset } from "./dataset";
import {
  VOICE_AUDIO_SPAN_ID,
  VOICE_AUDIO_TRACE_ID,
  VOICE_STORY_PROFILE,
  type VoiceFacts,
  voiceAudioTurn,
  voiceFacts,
} from "./story-voice";

const NOW = Date.parse("2026-11-19T12:00:00Z");
const LATER = Date.parse("2026-12-03T09:30:00Z");
const SEED = 1;
const TRACES = 2000;

function datasetAt(now: number): Dataset {
  return createDataset({ seed: SEED, now, traces: TRACES, profile: VOICE_STORY_PROFILE });
}

function factsAt(now: number): VoiceFacts {
  return voiceFacts(datasetAt(now));
}

/** The decoded RIFF/WAVE header and data size of a `data:audio/wav;base64` URI. */
function wavOf(uri: string) {
  const prefix = "data:audio/wav;base64,";
  expect(uri.startsWith(prefix)).toBe(true);
  const bytes = Uint8Array.from(atob(uri.slice(prefix.length)), (char) => char.charCodeAt(0));
  const view = new DataView(bytes.buffer);
  const text = (offset: number): string => String.fromCharCode(...bytes.slice(offset, offset + 4));
  return {
    riff: text(0),
    riffSize: view.getUint32(4, true),
    wave: text(8),
    fmt: text(12),
    channels: view.getUint16(22, true),
    sampleRate: view.getUint32(24, true),
    bits: view.getUint16(34, true),
    data: text(36),
    dataBytes: view.getUint32(40, true),
    fileBytes: bytes.length,
  };
}

const atNow = factsAt(NOW);
const atLater = factsAt(LATER);

// The pinned values are the screenshot clock's (NOW). A NaN (an empty set) fails every pin, so none can pass vacuously.
describe("the voice story data at the screenshot clock", () => {
  it("the availability checks have a median of 1,486 ms", () => {
    expect(atNow.availabilityMedianMs).toBe(1_486);
  });

  it("the availability checks have a p95 of 3,500 ms", () => {
    expect(atNow.availabilityP95Ms).toBe(3_500);
  });

  it("12 of the 17 hang-up calls (70.6%) had a slow turn before the last", () => {
    expect(atNow.hangupsAfterSlowTurn).toBeCloseTo(12 / 17, 10);
  });

  it("every slow turn before a hang-up ran an availability check of 2 s or more", () => {
    expect(atNow.slowTurnsWithAvailability).toBe(1);
  });

  it("1 of the 17 calls of the last 14 days was interrupted (5.9%)", () => {
    expect(atNow.interruptedCallShare).toBeCloseTo(1 / 17, 10);
  });

  it("the hourly p95 of the last 24 hours has a minimum of 3,335 ms", () => {
    expect(atNow.hourlyP95Ms).toHaveLength(24);
    expect(Math.min(...atNow.hourlyP95Ms)).toBe(3_335);
  });
});

describe("the voice story data keeps its ranges at the later clock", () => {
  it("the availability checks have a median between 1.2 s and 1.7 s", () => {
    expect(atLater.availabilityMedianMs).toBeGreaterThanOrEqual(1_200);
    expect(atLater.availabilityMedianMs).toBeLessThanOrEqual(1_700);
  });

  it("the availability checks have a p95 between 3.0 s and 3.7 s", () => {
    expect(atLater.availabilityP95Ms).toBeGreaterThanOrEqual(3_000);
    expect(atLater.availabilityP95Ms).toBeLessThanOrEqual(3_700);
  });

  it("71% of the hang-up calls had a slow turn before the last", () => {
    expect(Math.abs(atLater.hangupsAfterSlowTurn - 0.71)).toBeLessThanOrEqual(0.02);
  });

  it("at least 95% of those slow turns ran an availability check of 2 s or more", () => {
    expect(atLater.slowTurnsWithAvailability).toBeGreaterThanOrEqual(0.95);
  });

  it("about 6% of the calls of the last 14 days were interrupted", () => {
    expect(Math.abs(atLater.interruptedCallShare - 0.06)).toBeLessThanOrEqual(0.015);
  });

  it("every hourly p95 of the last 24 hours is above 1.5 s", () => {
    expect(atLater.hourlyP95Ms).toHaveLength(24);
    expect(atLater.p95AllDay).toBe(true);
  });
});

// The voice audio turn (P2) at the screenshot clock: the picture of the Chat tab shows its draft reply span.
describe("the voice audio turn", () => {
  const dataset = datasetAt(NOW);
  const index = voiceAudioTurn(dataset);
  const trace = dataset.traces[index];
  const draft = dataset.spanDetail(VOICE_AUDIO_TRACE_ID, VOICE_AUDIO_SPAN_ID);

  it("is the turn at index 110", () => {
    expect(index).toBe(110);
  });

  it("has the trace id and the draft reply span id the constants pin", () => {
    expect(trace?.trace_id).toBe(VOICE_AUDIO_TRACE_ID);
    expect(VOICE_AUDIO_TRACE_ID).toBe("a000000000000000000000000000006e");
    expect(VOICE_AUDIO_SPAN_ID).toBe("0000006e00000008");
  });

  it("started in the last 2 hours and carries the calendar tag", () => {
    const age = NOW - Date.parse(trace?.start_time ?? "");
    expect(age).toBeGreaterThanOrEqual(0);
    expect(age).toBeLessThanOrEqual(2 * 3_600_000);
    expect(trace?.tags).toContain("calendar");
    expect(trace?.tags).not.toContain("hangup");
  });

  it("has its draft reply span second to last and its availability check last", () => {
    const spans = dataset.traceDetail(VOICE_AUDIO_TRACE_ID)?.spans ?? [];
    const names = spans.slice(-2).map((row) => row.span.name);
    expect(names).toEqual(["draft reply", "check_availability"]);
    expect(draft?.name).toBe("draft reply");
    expect(draft?.span_kind).toBe("llm");
  });

  it("answers the caller's question with a tool call and its response, then the agent's audio", () => {
    const input = draft?.input?.format === "messages" ? draft.input.value : [];
    expect(input).toHaveLength(3);
    expect(input[0]?.role).toBe("user");
    expect(input[0]?.parts[0]).toMatchObject({
      type: "uri",
      modality: "audio",
      mime_type: "audio/wav",
      transcript: "Hi, can I move my appointment to Friday morning?",
    });
    expect(input[1]?.parts[0]).toMatchObject({
      type: "tool_call",
      id: "call_1",
      name: "check_availability",
      arguments: { day: "Friday", time: "morning" },
    });
    expect(input[2]?.role).toBe("tool");
    expect(input[2]?.parts[0]).toMatchObject({
      type: "tool_call_response",
      id: "call_1",
      response: { slots: ["9:30", "11:00"] },
    });
    const output = draft?.output?.format === "messages" ? draft.output.value : [];
    expect(output).toHaveLength(1);
    expect(output[0]?.parts[0]).toMatchObject({
      type: "uri",
      modality: "audio",
      mime_type: "audio/wav",
      transcript: "Friday at 9:30 is free. Should I book it?",
    });
  });

  it("has a caller WAV of 3.4 s and an agent WAV of 2.6 s, 8 kHz 16-bit mono", () => {
    const input = draft?.input?.format === "messages" ? draft.input.value : [];
    const output = draft?.output?.format === "messages" ? draft.output.value : [];
    const caller = input[0]?.parts[0];
    const agent = output[0]?.parts[0];
    const callerUri = caller && "uri" in caller && typeof caller.uri === "string" ? caller.uri : "";
    const agentUri = agent && "uri" in agent && typeof agent.uri === "string" ? agent.uri : "";

    const callerWav = wavOf(callerUri);
    expect(callerWav).toMatchObject({
      riff: "RIFF",
      wave: "WAVE",
      fmt: "fmt ",
      data: "data",
      channels: 1,
      sampleRate: 8_000,
      bits: 16,
      dataBytes: 54_400,
      riffSize: 54_436,
      fileBytes: 54_444,
    });

    const agentWav = wavOf(agentUri);
    expect(agentWav).toMatchObject({
      riff: "RIFF",
      wave: "WAVE",
      data: "data",
      channels: 1,
      sampleRate: 8_000,
      bits: 16,
      dataBytes: 41_600,
      riffSize: 41_636,
      fileBytes: 41_644,
    });
  });
});
