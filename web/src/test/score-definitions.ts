import { HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";

/** The four definitions of the annotation tests (05-m5-annotation.md § Fixed identifiers). */
export const ANNOTATION_DEFINITIONS: Schema<"ScoreDefinition">[] = [
  {
    id: "01900000-0000-7000-8000-000000000601",
    name: "helpfulness",
    data_type: "numeric",
    categories: [],
    min_value: 0,
    max_value: 1,
    description: "How helpful the answer is, from 0 to 1.",
    direction: "higher",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000604",
    name: "note",
    data_type: "text",
    categories: [],
    direction: "none",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000602",
    name: "resolved",
    data_type: "boolean",
    categories: [],
    description: "Did the answer resolve the question?",
    direction: "higher",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
  {
    id: "01900000-0000-7000-8000-000000000603",
    name: "tone",
    data_type: "categorical",
    categories: [
      { label: "friendly", value: 1 },
      { label: "neutral", value: 0.5 },
      { label: "rude", value: 0 },
    ],
    direction: "none",
    created_at: "2026-01-01T09:00:00.000000000Z",
    updated_at: "2026-01-01T09:00:00.000000000Z",
  },
];

/** Serves `definitions` as the project's score definitions, for any query. */
export function serveAnnotationDefinitions(
  definitions: Schema<"ScoreDefinition">[] = ANNOTATION_DEFINITIONS,
): void {
  server.use(
    http.get("/api/v1/projects/:project/score-definitions", () =>
      HttpResponse.json({ data: definitions, next_cursor: null }),
    ),
  );
}
