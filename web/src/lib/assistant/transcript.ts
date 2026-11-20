import type { AssistantMessage, AssistantPart, DraftKind, TurnEvent } from "@/lib/assistant/types";

export type Step = {
  id: string;
  name: string;
  arguments: unknown;
  result?: {
    ok: boolean;
    size_bytes: number;
    rows?: number;
    error_code?: string;
    stubbed?: boolean;
  };
};

export type Block =
  | { type: "text"; text: string }
  | { type: "step"; step: Step }
  | { type: "draft"; id: string; kind: DraftKind; draft: unknown };

export type Bubble =
  | { role: "user"; seq: number; text: string; context?: Record<string, unknown> }
  | {
      role: "assistant";
      seqs: number[];
      blocks: Block[];
      status: "complete" | "cancelled" | "failed" | "streaming";
      feedback?: "up" | "down";
      feedbackSeq?: number;
      traceId?: string;
    };

export type AssistantBubble = Extract<Bubble, { role: "assistant" }>;

/** A text block grows by `text`, or opens a new one after a non-text block. Returns a new array. */
function withText(blocks: Block[], text: string): Block[] {
  const last = blocks[blocks.length - 1];
  if (last?.type === "text") {
    return [...blocks.slice(0, -1), { type: "text", text: last.text + text }];
  }
  return [...blocks, { type: "text", text }];
}

/** Fills the step with `id` with its result. Returns a new array; an unknown id changes nothing. */
function withStepResult(blocks: Block[], id: string, result: NonNullable<Step["result"]>): Block[] {
  const index = blocks.findIndex((block) => block.type === "step" && block.step.id === id);
  const block = blocks[index];
  if (block?.type !== "step") {
    return blocks;
  }
  return blocks.map((item, position) =>
    position === index && item.type === "step"
      ? { type: "step", step: { ...item.step, result } }
      : item,
  );
}

function textOf(parts: AssistantPart[]): string {
  return parts.map((part) => (part.type === "text" ? part.content : "")).join("");
}

/**
 * Builds bubbles from stored messages: a user message is one bubble; the consecutive assistant and
 * tool messages after it form one assistant bubble. `status`, `feedback`, `feedbackSeq` and
 * `traceId` come from the bubble's last assistant message.
 */
export function bubblesOf(messages: AssistantMessage[]): Bubble[] {
  const bubbles: Bubble[] = [];
  let open: AssistantBubble | undefined;
  for (const message of messages) {
    if (message.role === "user") {
      open = undefined;
      bubbles.push({
        role: "user",
        seq: message.seq,
        text: textOf(message.parts),
        ...(message.context !== undefined ? { context: message.context } : {}),
      });
      continue;
    }
    if (open === undefined) {
      open = { role: "assistant", seqs: [], blocks: [], status: "complete" };
      bubbles.push(open);
    }
    open.seqs.push(message.seq);
    for (const part of message.parts) {
      switch (part.type) {
        case "text":
          open.blocks = withText(open.blocks, part.content);
          break;
        case "tool_call":
          open.blocks = [
            ...open.blocks,
            { type: "step", step: { id: part.id, name: part.name, arguments: part.arguments } },
          ];
          break;
        case "tool_result":
          open.blocks = withStepResult(open.blocks, part.id, {
            ok: part.ok,
            size_bytes: part.size_bytes,
            stubbed: part.stubbed,
            ...(part.rows !== undefined ? { rows: part.rows } : {}),
          });
          break;
        case "draft":
          open.blocks = [
            ...open.blocks,
            { type: "draft", id: part.id, kind: part.kind, draft: part.draft },
          ];
          break;
      }
    }
    if (message.role === "assistant") {
      open.status = message.status;
      open.feedbackSeq = message.seq;
      delete open.feedback;
      if (message.feedback !== undefined) {
        open.feedback = message.feedback;
      }
      delete open.traceId;
      if (message.turn_trace_id !== undefined) {
        open.traceId = message.turn_trace_id;
      }
    }
  }
  return bubbles;
}

/** Applies one live event to the streaming assistant bubble. Returns a new bubble. */
export function applyEvent(bubble: AssistantBubble, event: TurnEvent): AssistantBubble {
  switch (event.event) {
    case "start":
      return { ...bubble, traceId: event.data.trace_id };
    case "text":
      return { ...bubble, blocks: withText(bubble.blocks, event.data.delta) };
    case "tool_call":
      return {
        ...bubble,
        blocks: [
          ...bubble.blocks,
          {
            type: "step",
            step: { id: event.data.id, name: event.data.name, arguments: event.data.arguments },
          },
        ],
      };
    case "tool_result": {
      const { summary } = event.data;
      return {
        ...bubble,
        blocks: withStepResult(bubble.blocks, event.data.id, {
          ok: event.data.ok,
          size_bytes: summary.size_bytes,
          ...(summary.rows !== undefined ? { rows: summary.rows } : {}),
          ...(summary.error_code !== undefined ? { error_code: summary.error_code } : {}),
        }),
      };
    }
    case "draft":
      return {
        ...bubble,
        blocks: [
          ...bubble.blocks,
          { type: "draft", id: event.data.id, kind: event.data.kind, draft: event.data.draft },
        ],
      };
    case "done":
      return { ...bubble, status: "complete" };
    case "error":
      return { ...bubble, status: "failed" };
  }
}
