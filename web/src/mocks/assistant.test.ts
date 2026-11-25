import { beforeEach, describe, expect, it } from "vitest";
import {
  type AssistantBubble,
  applyEvent,
  type Bubble,
  bubblesOf,
} from "@/lib/assistant/transcript";
import {
  type AssistantMessage,
  parseDraft,
  parseTurnEvent,
  type TurnEvent,
} from "@/lib/assistant/types";
import {
  assistantHandlers,
  resetAssistantMock,
  STORIES,
  type Story,
  scriptFor,
} from "@/mocks/assistant";
import {
  createDataset,
  type Dataset,
  isEligibleTrace,
  type Profile,
} from "@/mocks/generated/dataset";
import { largeHandlers } from "@/mocks/generated/handlers";
import { CODING_STORY_PROFILE, codingFacts } from "@/mocks/generated/story-coding";
import { EXTRACTION_STORY_PROFILE, extractionFacts } from "@/mocks/generated/story-extraction";
import { SUPPORT_STORY_PROFILE, supportFacts } from "@/mocks/generated/story-support";
import { VOICE_STORY_PROFILE, voiceFacts } from "@/mocks/generated/story-voice";
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
import { STORY_DASHBOARD_IDS, storyDashboards } from "@/mocks/story-dashboards";

const OTHER_ID = "01900000-0000-7000-8000-000000001a01";
const NEW_ID = "01900000-0000-7000-8000-000000001a02";
const DEFAULT_TRACE_ID = "0af7651916cd43dd8448eb211c80319c";
const DEFAULT_TITLE = "Which trace errors spiked?";

/** The screenshot clock of the story datasets. */
const NOW = Date.parse("2026-11-19T12:00:00Z");
const HOUR_MS = 3_600_000;
const DAY_MS = 86_400_000;
const RANGE_MS: Record<string, number> = {
  "15m": 15 * 60_000,
  "1h": HOUR_MS,
  "6h": 6 * HOUR_MS,
  "24h": DAY_MS,
  "7d": 7 * DAY_MS,
  "30d": 30 * DAY_MS,
};

/** Each story's profile and trace count, as `main.tsx` builds the large mock (M17). */
const STORY_PROFILES: Record<StoryId, Profile> = {
  support: SUPPORT_STORY_PROFILE,
  coding: CODING_STORY_PROFILE,
  voice: VOICE_STORY_PROFILE,
  extraction: EXTRACTION_STORY_PROFILE,
};
const STORY_TRACES: Record<StoryId, number> = {
  support: 5_000,
  coding: 2_000,
  voice: 2_000,
  extraction: 2_000,
};

const datasets = new Map<StoryId, Dataset>();

/** A story's dataset at the screenshot clock, built with the story's profile. */
function storyDataset(story: StoryId): Dataset {
  const cached = datasets.get(story);
  if (cached !== undefined) {
    return cached;
  }
  const dataset = createDataset({
    seed: 1,
    now: NOW,
    traces: STORY_TRACES[story],
    profile: STORY_PROFILES[story],
  });
  datasets.set(story, dataset);
  return dataset;
}

/** The traces a story's facts count: not a showcase, not the big first trace, and visible. */
function eligibleOf(story: StoryId, dataset: Dataset) {
  return dataset.traces.filter((trace, index) =>
    isEligibleTrace(STORY_PROFILES[story], trace, index),
  );
}

function round2(value: number): number {
  return Math.round(value * 100) / 100;
}

function storyNamed(story: StoryId): Story {
  const found = STORIES.find((item) => item.story === story);
  if (found === undefined) {
    throw new Error(`no story ${story}`);
  }
  return found;
}

/** The assistant's API base in the story's project: `/api/v1/projects/<encoded ref>/assistant`. */
function baseOf(story: StoryId): string {
  const ref = encodeURIComponent(storyProject(story).ref);
  return `${window.location.origin}/api/v1/projects/${ref}/assistant`;
}

type StoredDetail = {
  id: string;
  title: string;
  message_count: number;
  messages: AssistantMessage[];
};

beforeEach(() => {
  resetAssistantMock();
  server.use(...assistantHandlers(0));
});

async function createChat(story: StoryId): Promise<void> {
  const response = await fetch(`${baseOf(story)}/conversations`, { method: "POST" });
  expect(response.status).toBe(201);
}

/** Runs one turn in the story's project and returns the events the stream sent, in order. */
async function ask(
  story: StoryId,
  id: string,
  message: string,
  context?: Record<string, unknown>,
): Promise<TurnEvent[]> {
  const response = await fetch(`${baseOf(story)}/conversations/${id}/turns`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ message, context }),
  });
  const text = await response.text();
  const events: TurnEvent[] = [];
  for (const block of text.split("\n\n")) {
    if (block.trim() === "" || block.startsWith(": open")) {
      continue;
    }
    const lines = block.split("\n");
    const name = lines.find((line) => line.startsWith("event: "))?.slice("event: ".length) ?? "";
    const data = lines.find((line) => line.startsWith("data: "))?.slice("data: ".length) ?? "";
    const event = parseTurnEvent(name, data);
    expect(event).not.toBeNull();
    if (event !== null) {
      events.push(event);
    }
  }
  return events;
}

async function detail(story: StoryId, id: string): Promise<StoredDetail> {
  const response = await fetch(`${baseOf(story)}/conversations/${id}`);
  return (await response.json()) as StoredDetail;
}

/** The SQL rows a stored story chat holds: the result of its one query call (message 3). */
async function storedRows(story: StoryId): Promise<Record<string, unknown>[]> {
  const stored = await detail(story, storyNamed(story).conversationId);
  const part = stored.messages[2]?.parts[0] as
    | { result?: { rows?: Record<string, unknown>[] } }
    | undefined;
  return part?.result?.rows ?? [];
}

/** The events of one kind, typed. */
function eventsNamed<N extends TurnEvent["event"]>(events: TurnEvent[], name: N) {
  return events.filter((event): event is Extract<TurnEvent, { event: N }> => event.event === name);
}

/** The bubble a live turn builds from its events, as the panel shows it before the reload. */
function streamedBubble(events: TurnEvent[]): AssistantBubble {
  return events.reduce<AssistantBubble>((bubble, event) => applyEvent(bubble, event), {
    role: "assistant",
    seqs: [],
    blocks: [],
    status: "streaming",
  });
}

/** A bubble with what the stored form and the streamed form must agree on. */
function comparable(bubble: AssistantBubble) {
  return {
    status: bubble.status,
    traceId: bubble.traceId,
    blocks: bubble.blocks.map((block) =>
      block.type === "step" && block.step.result !== undefined
        ? {
            ...block,
            step: {
              ...block.step,
              result: {
                ok: block.step.result.ok,
                size_bytes: block.step.result.size_bytes,
                rows: block.step.result.rows,
              },
            },
          }
        : block,
    ),
  };
}

function assistantAt(bubbles: Bubble[], index: number): AssistantBubble {
  const bubble = bubbles[index];
  if (bubble?.role !== "assistant") {
    throw new Error(`bubble ${index} is not an assistant bubble`);
  }
  return bubble;
}

function firstStory() {
  const [first] = STORIES;
  if (first === undefined) {
    throw new Error("no stories");
  }
  return first;
}

describe("assistant story conversations", () => {
  it("picks each story's script from the question, trimmed and in any case", async () => {
    for (const story of STORIES) {
      const [turn] = story.turns;
      await createChat(story.story);
      const exact = await ask(story.story, NEW_ID, turn.question);
      await createChat(story.story);
      const loose = await ask(story.story, NEW_ID, `  ${turn.question.toUpperCase()}  `);

      expect(loose).toEqual(exact);
      expect(eventsNamed(exact, "start")[0]?.data.trace_id).toBe(story.heroTraceId);
      expect(eventsNamed(exact, "tool_call")[0]?.data.name).toBe("query_sql");
      expect(eventsNamed(exact, "draft")[0]?.data.kind).toBe("filter");
      expect(exact.at(-1)).toMatchObject({ event: "done", data: { finish_reason: "stop" } });
    }
  });

  it("keeps today's script for any other question", async () => {
    await createChat("support");
    const events = await ask("support", NEW_ID, "hello");

    expect(events[0]).toEqual({
      event: "start",
      data: { turn_id: 1, trace_id: DEFAULT_TRACE_ID },
    });
    expect(eventsNamed(events, "tool_call").map((event) => event.data.name)).toEqual([
      "search_traces",
      "draft_filter",
    ]);
    expect(eventsNamed(events, "draft")[0]?.data.kind).toBe("filter");
    const texts = eventsNamed(events, "text");
    expect(texts.at(-1)?.data.delta.startsWith("Three traces failed in the last 24 hours")).toBe(
      true,
    );

    const stored = await detail("support", NEW_ID);
    expect(stored.title).toBe(DEFAULT_TITLE);
    expect(stored.message_count).toBe(6);
    expect(stored.messages.map((message) => message.seq)).toEqual([1, 2, 3, 4, 5, 6]);
    const [question] = stored.messages;
    expect(question?.parts).toEqual([
      { type: "text", content: "Which trace errors spiked in the last 24 hours?" },
    ]);
    expect(question?.context).toEqual({
      page: "traces",
      filter: "status = 'error'",
      range: "24h",
    });

    expect(scriptFor("").title).toBe(DEFAULT_TITLE);
  });

  it("stores the one turn of each story as it was streamed", async () => {
    for (const story of STORIES) {
      const [turn] = story.turns;
      await createChat(story.story);
      const events = await ask(story.story, NEW_ID, turn.question, {
        page: "traces",
        range: "24h",
      });

      expect(eventsNamed(events, "start")[0]?.data.turn_id).toBe(1);

      const stored = await detail(story.story, NEW_ID);
      expect(stored.title).toBe(turn.question);
      expect(stored.message_count).toBe(6);
      expect(stored.messages.map((message) => message.seq)).toEqual([1, 2, 3, 4, 5, 6]);
      expect(stored.messages.map((message) => message.role)).toEqual([
        "user",
        "assistant",
        "tool",
        "assistant",
        "tool",
        "assistant",
      ]);
      expect(stored.messages[0]?.parts).toEqual([{ type: "text", content: turn.question }]);
      expect(stored.messages[0]?.context).toEqual({ page: "traces", range: "24h" });

      const bubbles = bubblesOf(stored.messages);
      expect(bubbles.map((bubble) => bubble.role)).toEqual(["user", "assistant"]);
      expect(bubbles[0]).toMatchObject({ role: "user", text: turn.question });
      expect(comparable(assistantAt(bubbles, 1))).toEqual(comparable(streamedBubble(events)));
      expect(assistantAt(bubbles, 1).traceId).toBe(story.heroTraceId);
      expect(assistantAt(bubbles, 1).status).toBe("complete");

      const draftEvent = eventsNamed(events, "draft")[0];
      const storedDraft = assistantAt(bubbles, 1).blocks.find((block) => block.type === "draft");
      expect(draftEvent).toBeDefined();
      expect(storedDraft).toMatchObject({ type: "draft", draft: draftEvent?.data.draft });
    }
  });

  it("names only traces that exist, and the rows the query returned", async () => {
    for (const story of STORIES) {
      const ids = new Set(storyDataset(story.story).traces.map((trace) => trace.trace_id));
      expect(ids.has(story.heroTraceId)).toBe(true);

      const stored = await detail(story.story, story.conversationId);
      const search = stored.messages[2]?.parts[0] as { rows?: number } | undefined;
      const [turn] = story.turns;
      expect(search).toMatchObject({ rows: turn.rows });
      expect((await storedRows(story.story)).length).toBe(turn.rows);

      for (const message of stored.messages) {
        for (const part of message.parts) {
          if (part.type !== "text") {
            continue;
          }
          for (const match of part.content.matchAll(/\b[0-9a-f]{32}\b/g)) {
            expect(ids.has(match[0])).toBe(true);
          }
        }
      }
    }
  });

  it("lists each story chat in its project, after a new one and before the fixture chat", async () => {
    const listIn = async (story: StoryId) =>
      (await (await fetch(`${baseOf(story)}/conversations`)).json()) as {
        data: Record<string, unknown>[];
      };

    const support = await listIn("support");
    expect(support.data.map((item) => item.id)).toEqual([firstStory().conversationId, OTHER_ID]);
    expect(support.data.map((item) => item.message_count)).toEqual([6, 4]);
    for (const item of support.data) {
      expect(item).not.toHaveProperty("messages");
    }
    for (const story of STORIES.slice(1)) {
      const listed = await listIn(story.story);
      expect(listed.data.map((item) => item.id)).toEqual([story.conversationId]);
      expect(listed.data[0]?.title).toBe(story.turns[0].question);
      expect(listed.data[0]?.message_count).toBe(6);
    }

    await createChat("support");
    const empty = await listIn("support");
    expect(empty.data[0]).toMatchObject({ id: NEW_ID, title: "", message_count: 0 });

    await ask("support", NEW_ID, firstStory().turns[0].question);
    const after = await listIn("support");
    expect(after.data[0]).toMatchObject({
      id: NEW_ID,
      title: firstStory().turns[0].question,
      message_count: 6,
    });
  });

  it("opens a stored story chat directly", async () => {
    for (const story of STORIES) {
      const stored = await detail(story.story, story.conversationId);
      expect(stored.message_count).toBe(6);
      expect(stored.messages).toHaveLength(6);

      const bubbles = bubblesOf(stored.messages);
      expect(bubbles.map((bubble) => bubble.role)).toEqual(["user", "assistant"]);
      expect(bubbles[0]).toMatchObject({ role: "user", text: story.turns[0].question });

      const last = assistantAt(bubbles, 1);
      expect(last.traceId).toBe(story.heroTraceId);
      expect(last.blocks).toContainEqual(
        expect.objectContaining({ type: "draft", kind: "filter" }),
      );
    }

    const missing = await fetch(
      `${baseOf("support")}/conversations/01900000-0000-7000-8000-00000000ffff`,
    );
    expect(missing.status).toBe(404);
  });

  it("a story chat is not served in another project", async () => {
    const voice = STORIES.find((story) => story.story === "voice");
    if (voice === undefined) {
      throw new Error("no voice story");
    }
    const statusIn = async (slug: string) =>
      (
        await fetch(
          `${window.location.origin}/api/v1/projects/acme%2F${slug}/assistant/conversations/${voice.conversationId}`,
        )
      ).status;
    expect(await statusIn("code-agent")).toBe(404);
    expect(await statusIn("voice-desk")).toBe(200);
  });

  it("drafts a filter the Filter card accepts", () => {
    for (const story of STORIES) {
      const [turn] = story.turns;
      expect(turn.draft.kind).toBe("filter");
      const parsed = parseDraft("filter", turn.draft.draft);
      expect(parsed).not.toBeNull();
      expect(parsed?.kind).toBe("filter");
      expect(parsed?.kind === "filter" && parsed.draft.view).toBe("traces");
    }
  });

  it("each story stores its filter, then its story dashboard after the answer, and both parse", async () => {
    const dashboardIds: Record<StoryId, string> = {
      support: STORY_DASHBOARD_IDS.answerQuality,
      coding: STORY_DASHBOARD_IDS.codeAgentHealth,
      voice: STORY_DASHBOARD_IDS.voiceLatency,
      extraction: STORY_DASHBOARD_IDS.invoiceExtraction,
    };
    for (const story of STORIES) {
      const stored = await detail(story.story, story.conversationId);
      const drafts = stored.messages.flatMap((message) =>
        message.parts.flatMap((part) => (part.type === "draft" ? [part] : [])),
      );
      expect(drafts.map((part) => part.kind)).toEqual(["filter", "dashboard"]);

      const [filter, dashboard] = drafts;
      const parsedFilter = parseDraft("filter", filter?.draft);
      expect(parsedFilter?.kind).toBe("filter");
      expect(parsedFilter?.kind === "filter" && parsedFilter.draft.view).toBe("traces");

      const expected = storyDashboards().find((item) => item.id === dashboardIds[story.story]);
      const parsedDashboard = parseDraft("dashboard", dashboard?.draft);
      expect(parsedDashboard?.kind).toBe("dashboard");
      expect(parsedDashboard?.kind === "dashboard" && parsedDashboard.draft.name).toBe(
        expected?.name,
      );
      expect(parsedDashboard?.kind === "dashboard" && parsedDashboard.draft.definition).toEqual(
        expected?.definition,
      );
    }
  });

  it("the voice filter is the calendar tag alone", () => {
    expect(storyNamed("voice").turns[0].draft.draft.filter).toBe("tags @> ['calendar']");
  });

  it("each answer quotes the rounded facts of its story", () => {
    const answer = (story: StoryId) => storyNamed(story).turns[0].answer;

    const support = supportFacts(storyDataset("support"));
    expect(answer("support")).toContain(`**${Math.round(support.quotingShare * 100)}%**`);
    expect(answer("support")).toContain(`**${support.quotingOldPricing}**`);

    const coding = codingFacts(storyDataset("coding"));
    expect(answer("coding")).toContain(`**${Math.round(coding.retryShare * 100)}%**`);
    expect(answer("coding")).toContain(`**${Math.round(coding.retryCostShare * 100)}%**`);

    const voice = voiceFacts(storyDataset("voice"));
    expect(answer("voice")).toContain(`**${Math.round(voice.hangupsAfterSlowTurn * 100)}%**`);
    expect(answer("voice")).toContain(`**${(voice.availabilityMedianMs / 1000).toFixed(1)} s**`);
    expect(answer("voice")).toContain(`**${(voice.availabilityP95Ms / 1000).toFixed(1)} s**`);

    const extraction = extractionFacts(storyDataset("extraction"));
    expect(answer("extraction")).toContain(
      `**${Math.round(extraction.nordicFreightRetryShare * 100)}%**`,
    );
  });

  it("each query's rows are the facts' counts over the eligible traces", async () => {
    // Support: one release in the last 46 hours, with its feedback, downvotes and old-pricing traces.
    const supportSet = eligibleOf("support", storyDataset("support"));
    const sinceTuesday = NOW - 46 * HOUR_MS;
    const feedback = supportSet.filter(
      (trace) =>
        Date.parse(trace.start_time) >= sinceTuesday &&
        trace.scores.find((score) => score.name === "user_feedback")?.value.data_type === "boolean",
    );
    const downvotes = feedback.filter(
      (trace) =>
        trace.scores.find((score) => score.name === "user_feedback")?.value.data_type ===
          "boolean" &&
        trace.scores.find((score) => score.name === "user_feedback")?.value.value === false,
    );
    const support = supportFacts(storyDataset("support"));
    expect(await storedRows("support")).toEqual([
      {
        release: "v2.14",
        feedback: feedback.length,
        downvotes: support.downvotesSinceTuesday,
        old_pricing: support.quotingOldPricing,
      },
    ]);
    expect(downvotes.length).toBe(support.downvotesSinceTuesday);

    // Coding: runs in the last 14 hours, split by retry, with their cost.
    const codingRuns = eligibleOf("coding", storyDataset("coding")).filter(
      (trace) => Date.parse(trace.start_time) >= NOW - 14 * HOUR_MS,
    );
    const costOf = (runs: typeof codingRuns) =>
      round2(runs.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0));
    const notRetried = codingRuns.filter((trace) => !trace.tags.includes("api-retry"));
    const retried = codingRuns.filter((trace) => trace.tags.includes("api-retry"));
    const coding = codingFacts(storyDataset("coding"));
    const codingRows = await storedRows("coding");
    expect(codingRows).toEqual([
      { retried: false, runs: notRetried.length, cost: costOf(notRetried) },
      { retried: true, runs: retried.length, cost: costOf(retried) },
    ]);
    expect(Math.round((retried.length / codingRuns.length) * 100)).toBe(
      Math.round(coding.retryShare * 100),
    );

    // Voice: the calendar turns whose last span is a marked check_availability span.
    const voiceSet = storyDataset("voice");
    const checks = eligibleOf("voice", voiceSet).filter(
      (trace) =>
        trace.tags.includes("calendar") &&
        voiceSet.traceDetail(trace.trace_id)?.spans.at(-1)?.span.name === "check_availability",
    ).length;
    const voice = voiceFacts(voiceSet);
    expect(await storedRows("voice")).toEqual([
      {
        checks,
        p50_ms: Math.round(voice.availabilityMedianMs),
        p95_ms: Math.round(voice.availabilityP95Ms),
      },
    ]);
  });

  it("the extraction rows are the four vendors in order, with the facts' shares", async () => {
    // Extraction: the four comma vendors, grouped by their file name, with their eligible invoices and re-extraction shares.
    const extractionSet = storyDataset("extraction");
    const extraction = extractionFacts(extractionSet);
    const vendors: [string, string, number][] = [
      ["Nordic Freight", "invoice-2026-0521.pdf (2 pages)", extraction.nordicFreightRetryShare],
      ["Baltic Paper", "invoice-2026-0524.pdf (1 page)", extraction.commaVendors["Baltic Paper"]],
      ["Rhine Tools", "invoice-2026-0527.pdf (3 pages)", extraction.commaVendors["Rhine Tools"]],
      ["Lyon Bureau", "invoice-2026-0530.pdf (1 page)", extraction.commaVendors["Lyon Bureau"]],
    ];
    const extractionRows = await storedRows("extraction");
    expect(extractionRows.map((row) => row.vendor)).toEqual(vendors.map(([vendor]) => vendor));
    const eligible = eligibleOf("extraction", extractionSet);
    vendors.forEach(([vendor, file, share], index) => {
      const of = eligible.filter((trace) => trace.input_preview === file);
      expect(extractionRows[index]).toEqual({
        vendor,
        invoices: of.length,
        re_extracted: round2(share),
      });
      expect(round2(share)).toBe(
        round2(of.filter((trace) => trace.tags.includes("re-extracted")).length / of.length),
      );
    });
  });

  it("each story's SQL window, intro and filter range agree with the question", () => {
    const expected: Record<StoryId, { window: string; range: string; intro: string }> = {
      support: {
        window: "INTERVAL '46 hours'",
        range: "7d",
        intro: "I'll count the downvoted answers since Tuesday 14:00, by release. ",
      },
      coding: {
        window: "INTERVAL '14 hours'",
        range: "24h",
        intro: "I'll compare the runs since the cost rose last night with the day before.",
      },
      voice: {
        window: "INTERVAL '30 days'",
        range: "30d",
        intro: "I'll look at the turn before each hang-up over the last 30 days.",
      },
      extraction: {
        window: "INTERVAL '30 days'",
        range: "30d",
        intro: "I'll group the invoices by vendor.",
      },
    };
    for (const story of STORIES) {
      const want = expected[story.story];
      const [turn] = story.turns;
      expect(String(turn.call.arguments.query)).toContain(want.window);
      expect(turn.draft.draft.range).toBe(want.range);
      expect(turn.intro.join("")).toBe(want.intro);
    }
    expect(storyNamed("voice").turns[0].draft.draft.filter).toBe("tags @> ['calendar']");
  });

  it("each draft's filter lists the traces its answer counts", async () => {
    for (const story of STORIES) {
      const project = storyProject(story.story).ref;
      server.use(...largeHandlers(storyDataset(story.story), { project, datasets: false }));
      const draft = parseDraft("filter", story.turns[0].draft.draft);
      if (draft?.kind !== "filter") {
        throw new Error("draft did not parse as a filter");
      }
      const rangeMs = RANGE_MS[draft.draft.range ?? "24h"];
      if (rangeMs === undefined) {
        throw new Error("unknown range");
      }
      const query = new URLSearchParams({
        filter: draft.draft.filter ?? "",
        from: String(NOW - rangeMs),
        to: String(NOW),
        limit: "1000",
      });
      const listed = (await (
        await fetch(
          `${window.location.origin}/api/v1/projects/${encodeURIComponent(project)}/traces?${query}`,
        )
      ).json()) as { data: unknown[] };

      if (story.story === "support") {
        expect(listed.data.length).toBe(supportFacts(storyDataset("support")).quotingOldPricing);
      }
      if (story.story === "coding") {
        const retriedIn24h = eligibleOf("coding", storyDataset("coding")).filter(
          (trace) =>
            trace.tags.includes("api-retry") && Date.parse(trace.start_time) >= NOW - DAY_MS,
        ).length;
        expect(listed.data.length).toBe(retriedIn24h);
      }
    }
  });

  it("a new chat starts empty", async () => {
    await createChat("support");
    await ask("support", NEW_ID, firstStory().turns[0].question);
    await createChat("support");

    const stored = await detail("support", NEW_ID);
    expect(stored.messages).toEqual([]);
    expect(stored.message_count).toBe(0);
  });

  it("does not store a turn sent to another chat", async () => {
    const story = firstStory();
    await createChat("support");
    const events = await ask(story.story, story.conversationId, "hello");

    expect(events.length).toBeGreaterThan(0);
    expect((await detail("support", NEW_ID)).messages).toEqual([]);
    expect((await detail(story.story, story.conversationId)).message_count).toBe(6);
  });
});
