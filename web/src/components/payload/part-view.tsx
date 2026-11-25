import { useState } from "react";
import type { Schema } from "@/api/types";
import { JsonView } from "@/components/payload/json-view";
import { Markdown } from "@/components/payload/markdown";
import { MediaView } from "@/components/payload/media-view";
import { ToolCallLine } from "@/components/payload/tool-call-line";
import type { ToolLinks } from "@/lib/tool-calls";

export function PartView({
  part,
  projectRef,
  links,
}: {
  part: Schema<"Part">;
  projectRef?: string;
  links: ToolLinks;
}) {
  if (part.type === "text" && typeof part.content === "string") {
    return <Markdown text={part.content} />;
  }
  if (part.type === "reasoning" && typeof part.content === "string") {
    return <Reasoning content={part.content} />;
  }
  if (part.type === "tool_call") {
    const id = typeof part.id === "string" ? part.id : undefined;
    const name = typeof part.name === "string" ? part.name : "tool";
    const match = links.spans.get(part);
    return (
      <ToolCallLine
        name={name}
        id={id}
        args={{ value: part.arguments }}
        result={
          id !== undefined && links.responses.has(id)
            ? { value: links.responses.get(id) }
            : undefined
        }
        durationNs={match?.durationNs}
        error={match?.error}
      />
    );
  }
  if (part.type === "tool_call_response") {
    const id = typeof part.id === "string" ? part.id : undefined;
    // A response whose call is shown is the call's result, on the call's line.
    if (id !== undefined && links.folded.has(id)) return null;
    return (
      <ToolCallLine
        name={(id ? links.callNames.get(id) : undefined) ?? "response"}
        id={id}
        result={{ value: part.response }}
      />
    );
  }
  if (isUri(part) || isBlob(part)) {
    return <MediaView part={part} projectRef={projectRef} />;
  }
  return (
    <div className="flex flex-col gap-1">
      <p className="text-body text-muted-foreground">Unknown part</p>
      <JsonView value={part} />
    </div>
  );
}

function isUri(part: Schema<"Part">): part is Schema<"UriPart"> {
  return part.type === "uri" && typeof part.uri === "string";
}

function isBlob(part: Schema<"Part">): part is Schema<"BlobPart"> {
  return part.type === "blob" && typeof part.content === "string";
}

function Reasoning({ content }: { content: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="flex flex-col gap-1">
      <button
        type="button"
        className="self-start text-body text-muted-foreground"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        Reasoning · {content.length}
      </button>
      {open ? (
        <p className="text-body break-words whitespace-pre-wrap text-muted-foreground italic">
          {content}
        </p>
      ) : null}
    </div>
  );
}
