import { useMemo } from "react";
import type { Schema } from "@/api/types";
import { ChatView } from "@/components/payload/chat-view";
import { useProject } from "@/components/shell/project-context";
import { matchToolSpans } from "@/lib/tool-calls";

export function ChatTab({
  span,
  spans,
}: {
  span: Schema<"SpanDetail">;
  /** The trace's tree, for the tool spans that durations come from. */
  spans?: readonly Schema<"TreeSpan">[];
}) {
  const { ref } = useProject();
  const toolSpans = useMemo(() => {
    const row = spans?.find((candidate) => candidate.span.span_id === span.span_id);
    if (!spans || !row) return undefined;
    return matchToolSpans(row.span, spans, messagesOf(span.input), messagesOf(span.output));
  }, [span, spans]);
  return (
    <ChatView
      input={messagesOf(span.input)}
      output={messagesOf(span.output)}
      systemInstructions={span.system_instructions}
      toolDefinitions={span.tool_definitions}
      projectRef={ref}
      toolSpans={toolSpans}
    />
  );
}

function messagesOf(payload: Schema<"Payload"> | undefined): Schema<"Message">[] {
  if (payload?.format === "messages") return payload.value;
  return [];
}
