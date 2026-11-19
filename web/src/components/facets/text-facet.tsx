import { useState } from "react";
import type { FacetBodyProps } from "@/components/facets/facet-panel";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { readText, type TextOp, writeText } from "@/lib/facets";

const OPERATORS: { value: TextOp; label: string }[] = [
  { value: "equals", label: "equals" },
  { value: "contains", label: "contains" },
  { value: "starts", label: "starts with" },
  { value: "not_contains", label: "does not contain" },
];

/** The Text tab of a values facet, and the whole of a `text` facet such as `trace_id` (D3). */
export function TextFacet({ def, filter, onFilter }: FacetBodyProps) {
  const kind = def.kind === "text" ? "text" : "values";
  const initial = readText(filter, def.field, kind);
  // Keyed on what the filter says, so a Clear or an outside edit resets the form; typing does not.
  return (
    <TextForm
      key={JSON.stringify(initial ?? null)}
      label={def.label}
      kind={kind}
      initial={initial}
      onApply={(op, text) => onFilter(writeText(filter, def.field, kind, op, text))}
    />
  );
}

function TextForm({
  label,
  kind,
  initial,
  onApply,
}: {
  label: string;
  kind: "values" | "text";
  initial: ReturnType<typeof readText>;
  onApply: (op: TextOp, text: string) => void;
}) {
  const [op, setOp] = useState<TextOp>(initial?.op ?? "contains");
  const [text, setText] = useState(initial?.text ?? "");
  const mixed = initial?.mixed === true;
  const operators =
    kind === "text" ? OPERATORS : OPERATORS.filter((item) => item.value !== "equals");
  return (
    <form
      className="flex flex-col gap-1.5"
      onSubmit={(event) => {
        event.preventDefault();
        onApply(op, text);
      }}
    >
      <div className="flex min-w-0 gap-1">
        <select
          aria-label={`${label} operator`}
          className="h-7 shrink-0 rounded-md border border-input bg-transparent px-1 text-caption"
          value={op}
          disabled={mixed}
          onChange={(event) => setOp(event.target.value as TextOp)}
        >
          {operators.map((item) => (
            <option key={item.value} value={item.value}>
              {item.label}
            </option>
          ))}
        </select>
        <Input
          aria-label={`${label} text`}
          value={text}
          disabled={mixed}
          onChange={(event) => setText(event.target.value)}
          className="h-7 min-w-0 flex-1 shrink font-mono text-caption"
        />
      </div>
      {mixed ? <p className="text-caption text-muted-foreground">Edited as text</p> : null}
      <Button type="submit" size="xs" className="shrink-0 self-start" disabled={mixed}>
        Apply
      </Button>
    </form>
  );
}
