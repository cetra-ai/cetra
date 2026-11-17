import { cn } from "cn";
import { ChevronRight } from "lucide-react";
import { useState } from "react";
import type { Schema } from "@/api/types";
import { JsonView } from "@/components/payload/json-view";
import { PayloadView } from "@/components/payload/payload-view";

export function IoTab({ span }: { span: Schema<"SpanDetail"> }) {
  return (
    <div className="flex flex-col gap-4">
      {span.system_instructions !== undefined ? (
        <JsonSection label="System instructions" value={span.system_instructions} />
      ) : null}
      {span.tool_definitions !== undefined ? (
        <JsonSection label="Tool definitions" value={span.tool_definitions} />
      ) : null}
      <PayloadView label="Input" payload={span.input} />
      <PayloadView label="Output" payload={span.output} />
    </div>
  );
}

function JsonSection({ label, value }: { label: string; value: unknown }) {
  const [open, setOpen] = useState(false);
  return (
    <section className="flex flex-col gap-1">
      <button
        type="button"
        className="flex items-center gap-1 self-start text-[12px] font-medium"
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        <ChevronRight
          aria-hidden="true"
          className={cn("size-3.5 transition-transform", open && "rotate-90")}
        />
        {label}
      </button>
      {open ? <JsonView value={jsonValue(value)} /> : null}
    </section>
  );
}

/** A JSON string becomes a tree; anything else is shown as itself. */
function jsonValue(value: unknown): unknown {
  if (typeof value !== "string") {
    return value;
  }
  try {
    const parsed: unknown = JSON.parse(value);
    if (parsed !== null && typeof parsed === "object") {
      return parsed;
    }
  } catch {
    return value;
  }
  return value;
}
