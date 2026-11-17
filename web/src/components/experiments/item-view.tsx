import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import { JsonView } from "@/components/payload/json-view";
import { Markdown } from "@/components/payload/markdown";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { itemShortId, scoreValueText } from "@/lib/experiments";
import { formatCost, formatDuration } from "@/lib/format";

type ValueMode = "pretty" | "json";

/**
 * One value of an item: a string is Markdown, an object with one string field shows that string
 * as Markdown under its key, anything else is JSON. "Pretty / JSON" switches between the two; a
 * copy button copies the value (a string as it is, anything else as JSON).
 */
export function ValueView({ label, value }: { label: string; value: unknown }) {
  const [mode, setMode] = useState<ValueMode>("pretty");
  const missing = value === undefined;
  return (
    <section aria-label={label} className="flex min-w-0 flex-col gap-2">
      <div className="flex items-center gap-1">
        <h3 className="text-body font-medium text-muted-foreground">{label}</h3>
        <div className="ml-auto flex items-center gap-1">
          {missing ? null : (
            <>
              <Button
                type="button"
                variant={mode === "pretty" ? "secondary" : "ghost"}
                size="xs"
                aria-pressed={mode === "pretty"}
                onClick={() => {
                  setMode("pretty");
                }}
              >
                Pretty
              </Button>
              <Button
                type="button"
                variant={mode === "json" ? "secondary" : "ghost"}
                size="xs"
                aria-pressed={mode === "json"}
                onClick={() => {
                  setMode("json");
                }}
              >
                JSON
              </Button>
              <CopyButton
                label={label.toLowerCase()}
                value={typeof value === "string" ? value : JSON.stringify(value, null, 2)}
              />
            </>
          )}
        </div>
      </div>
      {missing ? (
        <span className="text-body text-muted-foreground">—</span>
      ) : mode === "json" ? (
        <JsonView value={value} />
      ) : (
        <PrettyValue value={value} />
      )}
    </section>
  );
}

function PrettyValue({ value }: { value: unknown }) {
  if (typeof value === "string") {
    return <Markdown text={value} />;
  }
  const field = singleStringField(value);
  if (field !== undefined) {
    return (
      <div className="flex flex-col gap-1">
        <span className="text-caption text-muted-foreground">{field.key}</span>
        <Markdown text={field.text} />
      </div>
    );
  }
  return <JsonView value={value} />;
}

/** The only field of a plain object, when that field is a string. */
function singleStringField(value: unknown): { key: string; text: string } | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return undefined;
  }
  const entries = Object.entries(value);
  const [only] = entries;
  if (entries.length !== 1 || only === undefined || typeof only[1] !== "string") {
    return undefined;
  }
  return { key: only[0], text: only[1] };
}

/** One result row: its input, expected output and output, then its error, scores and cost. */
export function ItemView({ row }: { row: Schema<"ResultRow"> }) {
  const { org, project } = useProject();
  const scores = Object.entries(row.scores);
  return (
    <div className="flex flex-col gap-5 p-4 text-ui">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-body text-muted-foreground">
        <span className="tabular-nums">{`Repetition ${row.repetition + 1}`}</span>
        <span className="tabular-nums">
          {row.latency_ns === undefined ? "Latency —" : `Latency ${formatDuration(row.latency_ns)}`}
        </span>
        <span className="tabular-nums">{`Cost ${formatCost(row.cost_total)}`}</span>
        {row.trace_id === undefined ? null : (
          <Link
            to="/$org/$project/traces/$traceId"
            params={{ org, project, traceId: row.trace_id }}
            className="rounded-sm text-body underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
          >
            Open trace
          </Link>
        )}
      </div>
      <div className="flex items-center gap-1 font-mono text-caption text-muted-foreground">
        <span className="break-all">{row.dataset_item_id}</span>
        <CopyButton label="item id" value={row.dataset_item_id} />
      </div>
      {row.error === undefined ? null : (
        <div
          role="alert"
          className="rounded-md border border-status-error px-3 py-2 text-body break-words whitespace-pre-wrap text-status-error"
        >
          {row.error}
        </div>
      )}
      <ValueView label="Input" value={row.input} />
      <ValueView label="Expected output" value={row.expected_output} />
      <ValueView label="Output" value={row.output} />
      <section aria-label="Scores" className="flex flex-col gap-2">
        <h3 className="text-body font-medium text-muted-foreground">Scores</h3>
        {scores.length === 0 ? (
          <span className="text-body text-muted-foreground">No scores</span>
        ) : (
          <ul className="flex flex-col gap-1">
            {scores.map(([name, value]) => (
              <li key={name} className="flex items-center gap-2">
                <span className="font-medium">{name}</span>
                <Badge variant="outline">{value.data_type}</Badge>
                <span className="tabular-nums">{scoreValueText(value)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

/** Keys in fields are left to the field; the arrows only step outside of them. */
function isEditable(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLElement &&
    (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName))
  );
}

/**
 * Up and down step through `onStep` while `open`, unless focus is in a field. Shared by the item
 * sheets of the experiment and compare pages.
 */
export function useArrowStep(open: boolean, onStep: (delta: 1 | -1) => void) {
  useEffect(() => {
    if (!open) {
      return;
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) {
        return;
      }
      if (isEditable(event.target)) {
        return;
      }
      if (event.key === "ArrowDown") {
        event.preventDefault();
        onStep(1);
      } else if (event.key === "ArrowUp") {
        event.preventDefault();
        onStep(-1);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [open, onStep]);
}

/**
 * The item view in a right-hand sheet. `row` undefined closes it. Up and down step to the
 * previous or next row through `onStep`, unless focus is in a field.
 */
export function ItemSheet({
  row,
  onStep,
  onClose,
}: {
  row: Schema<"ResultRow"> | undefined;
  onStep: (delta: 1 | -1) => void;
  onClose: () => void;
}) {
  const open = row !== undefined;
  useArrowStep(open, onStep);

  return (
    <Sheet
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          onClose();
        }
      }}
    >
      <SheetContent className="w-full overflow-y-auto sm:max-w-2xl">
        {row === undefined ? null : (
          <>
            <SheetHeader>
              <SheetTitle className="text-title">{`Item ${itemShortId(row.dataset_item_id)}`}</SheetTitle>
              <SheetDescription className="text-body">
                Up and down step to the other rows.
              </SheetDescription>
            </SheetHeader>
            <ItemView row={row} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
