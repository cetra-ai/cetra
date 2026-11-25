import { ChevronDown, ChevronRight, CircleX } from "lucide-react";
import { type ReactNode, useState } from "react";
import { IdText } from "@/components/id-text";
import { JsonView } from "@/components/payload/json-view";
import { formatDuration } from "@/lib/format";
import { inlineArgs, inlineResult } from "@/lib/tool-calls";

/** One tool call as a line: `name(args) → result · duration`. A click opens the arguments and result. */
export function ToolCallLine({
  name,
  id,
  args,
  result,
  durationNs,
  error,
  icon,
  onClick,
}: {
  name: string;
  /** The call id, shown when open. */
  id?: string;
  /** Absent for a response whose call is not shown: the line is `name → result`. */
  args?: { value: unknown };
  /** Absent when no result is known. */
  result?: { value: unknown };
  durationNs?: number;
  error?: string;
  /** Before the name, such as the Transcript's kind icon. */
  icon?: ReactNode;
  /** Called on every click of the line, after the open state toggles (the Transcript selects its span). */
  onClick?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const resultText = result !== undefined ? inlineResult(result.value) : "";
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <button
        type="button"
        aria-expanded={open}
        data-status={error !== undefined ? "error" : undefined}
        className="flex w-full min-w-0 items-center gap-1.5 text-left text-caption"
        onClick={() => {
          setOpen((current) => !current);
          onClick?.();
        }}
      >
        {open ? (
          <ChevronDown className="size-3 shrink-0 text-muted-foreground" aria-hidden />
        ) : (
          <ChevronRight className="size-3 shrink-0 text-muted-foreground" aria-hidden />
        )}
        {icon}
        <span className="min-w-0 truncate font-mono">
          <span className="font-medium">{name}</span>
          {args !== undefined ? `(${inlineArgs(args.value)})` : null}
          {error !== undefined ? (
            <>
              {" → "}
              <span className="text-destructive">
                <CircleX className="inline size-3" role="img" aria-label="Failed" />
                {error}
              </span>
            </>
          ) : resultText !== "" ? (
            ` → ${resultText}`
          ) : null}
          {durationNs !== undefined ? (
            <>
              {" "}
              <span className="num shrink-0 text-muted-foreground">
                {`· ${formatDuration(durationNs)}`}
              </span>
            </>
          ) : null}
        </span>
      </button>
      {open ? (
        <div className="flex flex-col gap-1 pl-4">
          {id ? <IdText value={id} kind="call" /> : null}
          {args !== undefined ? (
            <section aria-label="Arguments" className="flex flex-col gap-1">
              <p className="text-caption text-muted-foreground">Arguments</p>
              <ValueBody value={args.value} />
            </section>
          ) : null}
          {result !== undefined ? (
            <section aria-label="Result" className="flex flex-col gap-1">
              <p className="text-caption text-muted-foreground">Result</p>
              <ValueBody value={result.value} />
            </section>
          ) : null}
          {error !== undefined ? <p className="text-caption text-destructive">{error}</p> : null}
        </div>
      ) : null}
    </div>
  );
}

function ValueBody({ value }: { value: unknown }) {
  if (value === undefined) return null;
  if (typeof value === "string") {
    const parsed = jsonObject(value);
    if (parsed === undefined) {
      return <p className="text-caption break-words whitespace-pre-wrap">{value}</p>;
    }
    return <JsonView value={parsed} />;
  }
  return <JsonView value={value} />;
}

function jsonObject(value: string): unknown | undefined {
  try {
    const parsed: unknown = JSON.parse(value);
    if (parsed !== null && typeof parsed === "object") return parsed;
  } catch {
    return undefined;
  }
  return undefined;
}
