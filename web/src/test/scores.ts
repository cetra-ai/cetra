import { HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { ADA_ID, PROJECT_ID } from "@/test/fixtures";

/** The trace the score fixtures are attached to, unless a test overrides the target. */
export const SCORE_TRACE = "a1000000000000000000000000000001";
export const ADA_SCORE_ID = "01900000-0000-7000-8000-000000000310";

/** A human score of Ada on the test trace; `overrides` replace any field. */
export function scoreFor(overrides: Partial<Schema<"Score">>): Schema<"Score"> {
  const at = "2026-01-01T10:00:30.000000000Z";
  return {
    project_id: PROJECT_ID,
    score_id: ADA_SCORE_ID,
    target: { type: "trace", trace_id: SCORE_TRACE },
    session_id: "sess-001",
    user_id: "user-42",
    name: "helpfulness",
    value: { data_type: "numeric", value: 0.7 },
    source: "human",
    author_user_id: ADA_ID,
    created_at: at,
    updated_at: at,
    deleted: false,
    ingest_time: at,
    ...overrides,
  };
}

/** Serves `scores` for every score list and counts the reads. */
export function serveScores(scores: Schema<"Score">[]) {
  const reads = { count: 0 };
  server.use(
    http.get("/api/v1/projects/:project/scores", () => {
      reads.count += 1;
      return HttpResponse.json({ data: scores, next_cursor: null });
    }),
  );
  return reads;
}
