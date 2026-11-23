import type { ProviderChoice } from "@/lib/llm-adapters";

/**
 * Model ids the Default model field suggests for each provider choice (D-I-52). A short static
 * list: the field takes any other id too. OpenAI-compatible servers name their models
 * themselves, so they get none.
 */
export const MODEL_SUGGESTIONS: Record<ProviderChoice, readonly string[]> = {
  openai: [
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4o",
    "gpt-4o-mini",
    "o3",
    "o4-mini",
  ],
  openai_compatible: [],
  anthropic: ["claude-opus-4-7", "claude-sonnet-4-6", "claude-sonnet-4-5", "claude-haiku-4-5"],
  gemini: [
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
  ],
};
