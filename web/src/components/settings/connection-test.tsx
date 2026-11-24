import type { Schema } from "@/api/types";

const HINTS: Record<Schema<"TestErrorClass">, string> = {
  pause:
    "The key, quota or model access was refused. Rules that use this connection pause until a test of the default model passes.",
  retryable: "The provider did not answer in time or is overloaded. Try again.",
  permanent: "The request was rejected. Check the model name, base URL and structured output mode.",
};

/** The outcome of a tool-calling or connection test: the success line, or the failure with its hint. */
export function ConnectionTestResult({ result }: { result: Schema<"LlmConnectionTestResult"> }) {
  if (result.ok) {
    return (
      <p className="text-body">
        Connection works ({result.latency_ms} ms, model {result.model})
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-1">
      <p className="text-body font-medium text-destructive">Test failed</p>
      <p className="text-body">{result.error?.message}</p>
      {result.error ? (
        <p className="text-caption text-muted-foreground">{HINTS[result.error.class]}</p>
      ) : null}
    </div>
  );
}
