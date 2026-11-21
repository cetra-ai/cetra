export { type AiTelemetry, type AiTelemetryOptions, aiTelemetry } from "./ai.js";
export type {
  AddedQueueItems,
  AnnotationQueue,
  AnnotationQueueItem,
  AnnotationQueues,
  QueueCounts,
  QueueItemStatus,
  QueueTarget,
  QueueTargets,
  SkippedQueueItem,
} from "./annotation-queues.js";
export { ApiError, Client, type ClientOptions } from "./client.js";
export { type CetraOptions, ConfigError } from "./config.js";
export { type ContextOptions, withContext } from "./context.js";
export {
  Dataset,
  type DatasetInfo,
  type DatasetItem,
  type Datasets,
  type NewDatasetItem,
} from "./datasets.js";
export {
  type EvaluateOptions,
  ExperimentResult,
  evaluate,
  type Run,
  type Score,
  type Scorer,
  type ScorerArgs,
} from "./experiments.js";
export type { Instrumentation } from "./instrument.js";
export {
  type LlmSpanHandle,
  type LlmSpanOptions,
  type Usage,
  withLlmSpan,
} from "./llm.js";
export { Prompt, PromptNotAvailable, type PromptValues } from "./prompt.js";
export { type GetPromptOptions, getPrompt, type Prompts } from "./prompts.js";
export { flush, init, shutdown, spanProcessor, tracerProvider } from "./runtime.js";
export { type ScoreOptions, score } from "./scores.js";
export {
  currentSpanId,
  currentTraceId,
  type ObserveOptions,
  observe,
  type SpanHandle,
  type SpanKind,
  type SpanOptions,
  withSpan,
} from "./span.js";
export { type ChatEntry, type ChatMessage, PromptCompileError } from "./template.js";
export { uuid7 } from "./uuid.js";

/** Package version. Kept equal to package.json. */
export const VERSION = "0.1.0-rc.1";
