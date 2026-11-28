import { HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import connectionsFixture from "../../../fixtures/api/static/llm-connections.json";

/**
 * The LLM connections the story prompts run on, answered in the browser only. The fixture lists one
 * connection (OpenAI), which every column falls back to; the story prompts also run claude-* models
 * (Anthropic) and gemini-* models (Google), so the browser list adds one connection of each. The
 * endpoint is per organization, so every project of the organization sees the same list. Never
 * installed in tests; a test calls `server.use`.
 */

const openai = connectionsFixture.data[0] as Schema<"LlmConnection">;

/** The connection ids the story prompts name, by provider. */
export const STORY_CONNECTION_IDS = {
  openai: openai.id,
  anthropic: "01900000-0000-7000-8000-0000005709a1",
  google: "01900000-0000-7000-8000-0000005709a2",
} as const;

const anthropic: Schema<"LlmConnection"> = {
  id: STORY_CONNECTION_IDS.anthropic,
  name: "Anthropic",
  adapter: "anthropic",
  default_model: "claude-sonnet-4-5",
  structured_output: "tool",
  max_concurrency: 4,
  requests_per_minute: 300,
  api_key_hint: "Q7vX",
  extra_header_names: [],
  status: "ok",
  last_tested_at: "2026-01-01T09:06:00.000000000Z",
  last_test_ok: true,
  created_by: openai.created_by,
  created_at: "2026-01-01T09:01:00.000000000Z",
  updated_at: "2026-01-01T09:01:00.000000000Z",
};

const google: Schema<"LlmConnection"> = {
  id: STORY_CONNECTION_IDS.google,
  name: "Google",
  adapter: "gemini",
  default_model: "gemini-2.5-flash",
  structured_output: "native",
  max_concurrency: 4,
  requests_per_minute: 300,
  api_key_hint: "M3dR",
  extra_header_names: [],
  status: "ok",
  last_tested_at: "2026-01-01T09:07:00.000000000Z",
  last_test_ok: true,
  created_by: openai.created_by,
  created_at: "2026-01-01T09:02:00.000000000Z",
  updated_at: "2026-01-01T09:02:00.000000000Z",
};

/** The organization's connections: the fixture's OpenAI first, then Anthropic and Google. */
export const STORY_CONNECTIONS: Schema<"LlmConnection">[] = [openai, anthropic, google];

/**
 * The connection a story version's model runs on, by the model's name: `claude` for Anthropic,
 * `gemini` for Google and `gpt` for OpenAI. `undefined` for any other model.
 */
export function connectionIdOf(model: string): string | undefined {
  if (model.startsWith("claude")) {
    return STORY_CONNECTION_IDS.anthropic;
  }
  if (model.startsWith("gemini")) {
    return STORY_CONNECTION_IDS.google;
  }
  if (model.startsWith("gpt")) {
    return STORY_CONNECTION_IDS.openai;
  }
  return undefined;
}

export const storyConnectionHandlers = [
  http.get("/api/v1/orgs/:org/llm-connections", () =>
    HttpResponse.json({ data: STORY_CONNECTIONS, next_cursor: null }),
  ),
];
