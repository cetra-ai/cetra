import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { ADA_ID, BOB_ID } from "@/test/fixtures";
import { ANNOTATION_DEFINITIONS } from "@/test/score-definitions";
import {
  type AnnotationTarget,
  type Draft,
  draftError,
  draftOf,
  EMPTY_DRAFT,
  invalidateScoreReads,
  isDirty,
  onTarget,
  ownScores,
  rowErrors,
  scoreItem,
  scoreTarget,
  targetKey,
} from "./annotation";

type Score = Schema<"Score">;
type Definition = Schema<"ScoreDefinition">;

const TRACE_1 = "a1000000000000000000000000000001";
const SPAN_4 = "b100000000000014";
const SESSION = "sess-001";
const QUEUE = "01950000-0000-7000-8000-000000000a01";

/** The annotation test definition called `name`. */
function definition(name: string): Definition {
  const found = ANNOTATION_DEFINITIONS.find((item) => item.name === name);
  if (found === undefined) {
    throw new Error(`no test definition ${name}`);
  }
  return found;
}

const trace1: AnnotationTarget = { type: "trace", traceId: TRACE_1 };
const span4: AnnotationTarget = { type: "span", traceId: TRACE_1, spanId: SPAN_4 };
const session: AnnotationTarget = { type: "session", sessionId: SESSION };

let counter = 0;

/** A live `human` helpfulness score of Ada on trace 1, with `overrides` applied. */
function score(overrides: Partial<Score> = {}): Score {
  counter += 1;
  return {
    project_id: "01900000-0000-7000-8000-000000000001",
    score_id: `01900000-0000-7000-8000-${String(counter).padStart(12, "0")}`,
    target: { type: "trace", trace_id: TRACE_1 },
    session_id: SESSION,
    user_id: "user-42",
    name: "helpfulness",
    value: { data_type: "numeric", value: 0.7 },
    source: "human",
    author_user_id: ADA_ID,
    created_at: "2026-01-01T10:00:30.000000000Z",
    updated_at: "2026-01-01T10:00:30.000000000Z",
    deleted: false,
    ingest_time: "2026-01-01T10:00:30.000000000Z",
    ...overrides,
  };
}

const draft = (value: string, reason = ""): Draft => ({ value, reason });

describe("targets", () => {
  it("keys and converts the three targets", () => {
    expect(targetKey(trace1)).toBe(`trace:${TRACE_1}`);
    expect(targetKey(span4)).toBe(`span:${TRACE_1}:${SPAN_4}`);
    expect(targetKey(session)).toBe(`session:${SESSION}`);
    expect(scoreTarget(trace1)).toEqual({ type: "trace", trace_id: TRACE_1 });
    expect(scoreTarget(span4)).toEqual({ type: "span", trace_id: TRACE_1, span_id: SPAN_4 });
    expect(scoreTarget(session)).toEqual({ type: "session", session_id: SESSION });
  });

  it("matches a score to its own kind and ids only", () => {
    expect(onTarget(score(), trace1)).toBe(true);
    expect(onTarget(score(), span4)).toBe(false);
    expect(
      onTarget(score({ target: { type: "span", trace_id: TRACE_1, span_id: SPAN_4 } }), span4),
    ).toBe(true);
    expect(onTarget(score({ target: { type: "session", session_id: SESSION } }), session)).toBe(
      true,
    );
  });
});

describe("ownScores", () => {
  it("keeps the caller's live human scores on the target", () => {
    const kept = score({ name: "helpfulness" });
    const scores = [
      kept,
      score({
        name: "resolved",
        value: { data_type: "boolean", value: true },
        author_user_id: BOB_ID,
      }),
      score({ name: "tone", source: "api", author_user_id: undefined }),
      score({ name: "note", deleted: true, value: { data_type: "text", value: "x" } }),
      score({
        name: "helpfulness",
        target: { type: "span", trace_id: TRACE_1, span_id: SPAN_4 },
      }),
      score({ name: "resolved", value: { data_type: "boolean", value: false } }),
    ];

    expect([...ownScores(scores, trace1, ADA_ID).keys()].sort()).toEqual([
      "helpfulness",
      "resolved",
    ]);
    expect(ownScores(scores, trace1, ADA_ID).get("helpfulness")).toBe(kept);
    expect([...ownScores(scores, span4, ADA_ID).keys()]).toEqual(["helpfulness"]);
  });

  it("keeps Ada's human score on the session target", () => {
    const onSession = score({
      name: "resolved",
      target: { type: "session", session_id: SESSION },
      value: { data_type: "boolean", value: true },
    });
    expect(ownScores([onSession], session, ADA_ID).get("resolved")).toBe(onSession);
    expect(ownScores([onSession], trace1, ADA_ID).size).toBe(0);
  });

  it("keeps the newer of two scores with one name", () => {
    const older = score({
      name: "note",
      value: { data_type: "text", value: "old" },
      updated_at: "2026-01-01T10:00:00.000000000Z",
    });
    const newer = score({
      name: "note",
      value: { data_type: "text", value: "new" },
      updated_at: "2026-01-01T11:00:00.000000000Z",
    });
    expect(ownScores([newer, older], trace1, ADA_ID).get("note")).toBe(newer);
    expect(ownScores([older, newer], trace1, ADA_ID).get("note")).toBe(newer);
  });
});

describe("draftOf", () => {
  it("renders each type", () => {
    expect(draftOf(score({ reason: "Solid" }), definition("helpfulness"))).toEqual({
      value: "0.7",
      reason: "Solid",
    });
    expect(
      draftOf(
        score({ name: "resolved", value: { data_type: "boolean", value: false } }),
        definition("resolved"),
      ).value,
    ).toBe("false");
    expect(
      draftOf(
        score({
          name: "tone",
          value: { data_type: "categorical", value: { label: "neutral", value: 0.5 } },
        }),
        definition("tone"),
      ).value,
    ).toBe("neutral");
    expect(
      draftOf(
        score({ name: "note", value: { data_type: "text", value: "hello" } }),
        definition("note"),
      ),
    ).toEqual({ value: "hello", reason: "" });
  });

  it("is empty for no score or a type mismatch", () => {
    expect(draftOf(undefined, definition("helpfulness"))).toBe(EMPTY_DRAFT);
    expect(draftOf(score(), definition("resolved"))).toBe(EMPTY_DRAFT);
  });
});

describe("isDirty", () => {
  it("compares the value and the trimmed reason", () => {
    const saved = draft("0.7", "Solid");
    expect(isDirty(draft("0.7", "Solid  "), saved)).toBe(false);
    expect(isDirty(draft("0.8", "Solid"), saved)).toBe(true);
    expect(isDirty(draft("0.7", "Solid!"), saved)).toBe(true);
  });
});

describe("draftError", () => {
  it("checks numbers against their range", () => {
    const helpfulness = definition("helpfulness");
    expect(draftError(draft("abc"), helpfulness)).toBe("Enter a number.");
    expect(draftError(draft("0x10"), helpfulness)).toBe("Enter a number.");
    expect(draftError(draft("0b1"), helpfulness)).toBe("Enter a number.");
    expect(draftError(draft(" 0.5 "), helpfulness)).toBeUndefined();
    expect(draftError(draft("1.5"), helpfulness)).toBe("Enter a number from 0 to 1.");
    expect(draftError(draft("0.5"), helpfulness)).toBeUndefined();

    const atLeast = { ...helpfulness, max_value: undefined };
    expect(draftError(draft("-1"), atLeast)).toBe("Enter a number of at least 0.");

    const atMost = { ...helpfulness, min_value: undefined };
    expect(draftError(draft("2"), atMost)).toBe("Enter a number of at most 1.");
  });

  it("checks booleans and categories", () => {
    expect(draftError(draft("maybe"), definition("resolved"))).toBe("Choose Yes or No.");
    expect(draftError(draft("true"), definition("resolved"))).toBeUndefined();
    expect(draftError(draft("grumpy"), definition("tone"))).toBe("Choose one of the categories.");
    expect(draftError(draft("rude"), definition("tone"))).toBeUndefined();
  });

  it("accepts an unset row and any text", () => {
    expect(draftError(draft(""), definition("helpfulness"))).toBeUndefined();
    expect(draftError(draft("  "), definition("resolved"))).toBeUndefined();
    expect(draftError(draft("anything at all"), definition("note"))).toBeUndefined();
  });
});

describe("scoreItem", () => {
  it("builds a trace numeric item with the session and the user", () => {
    expect(
      scoreItem(definition("helpfulness"), draft("0.8"), trace1, {
        sessionId: SESSION,
        userId: "user-42",
      }),
    ).toEqual({
      target: { type: "trace", trace_id: TRACE_1 },
      session_id: SESSION,
      user_id: "user-42",
      name: "helpfulness",
      value: { data_type: "numeric", value: 0.8 },
    });
  });

  it("builds a span boolean item", () => {
    expect(scoreItem(definition("resolved"), draft("false"), span4, {})).toEqual({
      target: { type: "span", trace_id: TRACE_1, span_id: SPAN_4 },
      name: "resolved",
      value: { data_type: "boolean", value: false },
    });
  });

  it("carries a category's number when it has one", () => {
    expect(scoreItem(definition("tone"), draft("neutral"), trace1, {}).value).toEqual({
      data_type: "categorical",
      value: { label: "neutral", value: 0.5 },
    });
    const withoutNumber: Definition = {
      ...definition("tone"),
      categories: [{ label: "plain" }],
    };
    expect(scoreItem(withoutNumber, draft("plain"), trace1, {}).value).toEqual({
      data_type: "categorical",
      value: { label: "plain" },
    });
  });

  it("gives a session target no session id", () => {
    const item = scoreItem(definition("resolved"), draft("true"), session, {
      sessionId: SESSION,
      userId: "user-42",
    });
    expect("session_id" in item).toBe(false);
    expect(item.user_id).toBe("user-42");
    expect(item.target).toEqual({ type: "session", session_id: SESSION });
  });

  it("trims the reason and drops an empty one", () => {
    expect(scoreItem(definition("note"), draft("hi", "  ok  "), trace1, {}).reason).toBe("ok");
    expect("reason" in scoreItem(definition("note"), draft("hi", "   "), trace1, {})).toBe(false);
  });

  it("never sends a score id or a source", () => {
    const item = scoreItem(definition("helpfulness"), draft("0.1"), trace1, {});
    expect("score_id" in item).toBe(false);
    expect("source" in item).toBe(false);
  });

  it("sends the queue id when the context has one", () => {
    const item = scoreItem(definition("helpfulness"), draft("0.1"), trace1, { queueId: QUEUE });
    expect(item.queue_id).toBe(QUEUE);
  });

  it("leaves the queue id out without a queue", () => {
    const item = scoreItem(definition("helpfulness"), draft("0.1"), trace1, {});
    expect("queue_id" in item).toBe(false);
  });
});

describe("rowErrors", () => {
  const sentIds = ["01900000-0000-7000-8000-000000000601", "01900000-0000-7000-8000-000000000604"];

  it("maps each field error to the definition sent at its index", () => {
    const error = {
      type: "about:blank",
      title: "Bad Request",
      status: 400,
      code: "bad_request",
      errors: [
        { field: "scores[1].value", message: "First problem" },
        { field: "scores[1].name", message: "Second problem" },
      ],
    };
    expect(rowErrors(error, sentIds)).toEqual({ [sentIds[1] as string]: "First problem" });
  });

  it("gives nothing for an error that is not a problem", () => {
    expect(rowErrors(new Error("network"), sentIds)).toEqual({});
  });
});

describe("invalidateScoreReads", () => {
  it("invalidates only the score reads", async () => {
    const queryClient = new QueryClient();
    const scores = ["get", "/api/v1/projects/{project}/scores", {}] as const;
    const traceDetail = ["get", "/api/v1/projects/{project}/traces/{trace_id}", {}] as const;
    const evaluators = ["get", "/api/v1/projects/{project}/evaluators", {}] as const;
    queryClient.setQueryData(scores, { data: [], next_cursor: null });
    queryClient.setQueryData(traceDetail, { trace: {} });
    queryClient.setQueryData(evaluators, { data: [], next_cursor: null });

    await invalidateScoreReads(queryClient);

    expect(queryClient.getQueryState(scores)?.isInvalidated).toBe(true);
    expect(queryClient.getQueryState(traceDetail)?.isInvalidated).toBe(true);
    expect(queryClient.getQueryState(evaluators)?.isInvalidated).toBe(false);
  });
});
