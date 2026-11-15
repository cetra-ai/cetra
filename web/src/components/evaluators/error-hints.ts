import type { Schema } from "@/api/types";

/** What a failed evaluator run means for the person who ran it. */
export const ERROR_HINTS: Record<Schema<"TestErrorClass">, string> = {
  pause:
    "The key, quota or model access was refused. Check the connection in Settings › LLM connections.",
  retryable: "The provider or webhook did not answer in time or is overloaded. Try again.",
  permanent:
    "The request was rejected or the answer did not fit. Check the model, the prompt and the output type.",
};
