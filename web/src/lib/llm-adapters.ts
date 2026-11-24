import type { Schema } from "@/api/types";

/** The connection form's four provider choices over the three adapters (D-I-51). */
export type ProviderChoice = "openai" | "openai_compatible" | "anthropic" | "gemini";

export const PROVIDER_CHOICES: readonly { value: ProviderChoice; label: string }[] = [
  { value: "openai", label: "OpenAI" },
  { value: "openai_compatible", label: "OpenAI-compatible" },
  { value: "anthropic", label: "Anthropic" },
  { value: "gemini", label: "Gemini" },
];

/** The choice a connection shows as: `openai` with a base URL is OpenAI-compatible. */
export function providerChoice(connection: {
  adapter: Schema<"LlmAdapter">;
  base_url?: string | null;
}): ProviderChoice {
  if (connection.adapter === "openai") {
    return connection.base_url ? "openai_compatible" : "openai";
  }
  return connection.adapter;
}

/** The provider's display name: OpenAI, OpenAI-compatible, Anthropic or Gemini. */
export function providerLabel(connection: {
  adapter: Schema<"LlmAdapter">;
  base_url?: string | null;
}): string {
  const choice = providerChoice(connection);
  return PROVIDER_CHOICES.find((option) => option.value === choice)?.label ?? choice;
}

/** The adapter a choice saves: OpenAI-compatible is `openai` with a base URL. */
export function adapterOf(choice: ProviderChoice): Schema<"LlmAdapter"> {
  return choice === "openai_compatible" ? "openai" : choice;
}

/** Each adapter's default base URL (`DEFAULT_BASE` in `crates/eval/src/chat/*.rs`). */
export const DEFAULT_BASE_URLS: Record<Schema<"LlmAdapter">, string> = {
  openai: "https://api.openai.com/v1",
  anthropic: "https://api.anthropic.com",
  gemini: "https://generativelanguage.googleapis.com",
};
