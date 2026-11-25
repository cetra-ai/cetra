import { expect, it } from "vitest";
import { MODEL_SUGGESTIONS } from "@/lib/llm-models";

it("suggests no models for an OpenAI-compatible server", () => {
  expect(MODEL_SUGGESTIONS.openai_compatible).toEqual([]);
});

it("suggests unique, valid model ids for each other provider", () => {
  for (const choice of ["openai", "anthropic", "gemini"] as const) {
    const ids = MODEL_SUGGESTIONS[choice];
    expect(ids.length, choice).toBeGreaterThan(0);
    expect(new Set(ids).size, choice).toBe(ids.length);
    for (const id of ids) {
      expect(id, choice).not.toMatch(/\s/);
      expect(id, choice).not.toMatch(/[/\\%?#]/);
    }
  }
});
