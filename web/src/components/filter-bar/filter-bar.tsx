import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { X } from "lucide-react";
import { type FormEvent, type ReactNode, useState } from "react";
import type { Problem } from "@/api/problem";
import { schemaQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useShortcut } from "@/components/command/use-shortcut";
import type { FacetScope } from "@/components/facets/facet-panel";
import { FilterEditor } from "@/components/filter-bar/editor";
import { FieldPicker } from "@/components/filter-bar/field-picker";
import {
  chipFromDraft,
  chipOperators,
  type Draft,
  draftFromChip,
  facetTarget,
  facetViewOf,
  fieldOf,
  filterErrorRange,
  filterMessage,
  isListOp,
  keepsName,
  listTokens,
  listValueText,
  needsValue,
  targetKind,
} from "@/components/filter-bar/logic";
import { type Suggest, ValueInput } from "@/components/filter-bar/value-input";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  appendFilter,
  type Chip,
  parseChip,
  serializeChip,
  serializeFilter,
  splitConjuncts,
} from "@/lib/dsl/dsl";

type ViewName = Schema<"ViewName">;

/**
 * The filter text of a list as chips, with the text editor behind them. `scope` is the list's
 * window and flags: value suggestions are fetched with it. Without it there are none (D7).
 */
export function FilterBar({
  view,
  value,
  onApply,
  error,
  placeholder,
  scope,
}: {
  view: ViewName;
  value: string;
  onApply: (text: string) => void;
  error?: Problem;
  placeholder?: string;
  scope?: FacetScope;
}) {
  const { org, project } = useProject();
  const schema = useQuery(schemaQuery(org, project));
  const [textMode, setTextMode] = useState<{ from?: number; to?: number } | null>(null);
  const columns = schema.data?.views.find((item) => item.name === view)?.columns ?? [];
  const showText = textMode !== null || error !== undefined;
  const errorRange = error ? filterErrorRange(value, error) : null;
  const selection =
    textMode?.from !== undefined && textMode.to !== undefined
      ? { from: textMode.from, to: textMode.to }
      : undefined;

  useShortcut("filter-text", (event) => {
    if (showText) {
      return;
    }
    event.preventDefault();
    setTextMode({});
  });

  function apply(text: string) {
    setTextMode(null);
    onApply(text);
  }

  const conjuncts = splitConjuncts(value).map((part) => ({ ...part, chip: parseChip(part.text) }));

  return (
    <div className="flex min-w-0 flex-col">
      <div
        className={cn(
          "flex min-h-7 min-w-0 items-center gap-1 rounded-md border bg-background px-1 py-0.5",
          error ? "border-destructive" : "border-input",
        )}
      >
        {showText ? (
          <FilterEditor
            value={value}
            selection={selection}
            errorRange={errorRange}
            viewName={view}
            schema={schema.data}
            scope={scope}
            placeholder={placeholder}
            onApply={apply}
            onCancel={() => {
              if (!error) {
                setTextMode(null);
              }
            }}
          />
        ) : (
          <Chips
            value={value}
            conjuncts={conjuncts}
            columns={columns}
            view={view}
            scope={scope}
            onApply={apply}
            placeholder={placeholder}
            onEditText={(range) => setTextMode(range ?? {})}
          />
        )}
      </div>
      {error ? (
        <p className="text-body leading-4 break-words text-destructive">{filterMessage(error)}</p>
      ) : null}
    </div>
  );
}

function Chips({
  value,
  conjuncts,
  columns,
  view,
  scope,
  onApply,
  placeholder = "Filter…",
  onEditText,
}: {
  value: string;
  conjuncts: { text: string; start: number; end: number; chip: Chip }[];
  columns: Schema<"ColumnSchema">[];
  view: ViewName;
  scope?: FacetScope;
  onApply: (text: string) => void;
  placeholder?: string;
  onEditText: (range?: { from: number; to: number }) => void;
}) {
  function replace(index: number, chip: Chip) {
    onApply(serializeFilter(conjuncts.map((part, i) => (i === index ? chip : part.chip))));
  }
  function remove(index: number) {
    onApply(serializeFilter(conjuncts.filter((_, i) => i !== index).map((part) => part.chip)));
  }
  return (
    <div className="flex min-w-0 flex-1 flex-wrap items-center gap-1">
      {conjuncts.map((part, index) =>
        part.chip.kind === "raw" ? (
          <ChipShell
            key={`${part.start}:${part.text}`}
            text={part.text}
            onRemove={() => remove(index)}
          >
            <button
              type="button"
              className="focus-ring truncate rounded-sm px-1.5"
              onClick={() => onEditText({ from: part.start, to: part.end })}
            >
              {part.text}
            </button>
          </ChipShell>
        ) : (
          <ChipEditor
            key={`${part.start}:${part.text}`}
            text={part.text}
            chip={part.chip}
            columns={columns}
            view={view}
            scope={scope}
            onRemove={() => remove(index)}
            onApply={(chip) => replace(index, chip)}
          />
        ),
      )}
      <button
        type="button"
        aria-label="Edit filter"
        className="focus-ring h-6 min-w-8 flex-1 truncate rounded-sm px-1 text-left text-caption text-muted-foreground"
        onClick={() => onEditText()}
      >
        {conjuncts.length === 0 ? placeholder : ""}
      </button>
      <AddChip value={value} columns={columns} view={view} scope={scope} onApply={onApply} />
      <button
        type="button"
        className="focus-ring shrink-0 rounded-sm px-1 text-caption text-muted-foreground"
        onClick={() => onEditText()}
      >
        Edit as text
      </button>
    </div>
  );
}

/** "+ Filter": an empty chip form in a popover; its chip is appended to the filter text (D7). */
function AddChip({
  value,
  columns,
  view,
  scope,
  onApply,
}: {
  value: string;
  columns: Schema<"ColumnSchema">[];
  view: ViewName;
  scope?: FacetScope;
  onApply: (text: string) => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="focus-ring shrink-0 rounded-sm px-1 text-caption text-muted-foreground"
        >
          + Filter
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 gap-2 p-3">
        <ChipForm
          initial={{ target: columns[0]?.name ?? "", name: "", op: "=", value: "" }}
          columns={columns}
          view={view}
          scope={scope}
          onApply={(chip) => {
            onApply(appendFilter(value, serializeChip(chip)));
            setOpen(false);
          }}
        />
      </PopoverContent>
    </Popover>
  );
}

function ChipShell({
  text,
  onRemove,
  children,
}: {
  text: string;
  onRemove: () => void;
  children: ReactNode;
}) {
  return (
    <span className="inline-flex h-6 max-w-64 shrink-0 items-center rounded-md bg-muted text-caption">
      {children}
      <button
        type="button"
        aria-label={`Remove ${text}`}
        className="focus-ring rounded-sm px-1 text-muted-foreground"
        onClick={(event) => {
          event.stopPropagation();
          onRemove();
        }}
      >
        <X className="size-3" />
      </button>
    </span>
  );
}

function ChipEditor({
  text,
  chip,
  columns,
  view,
  scope,
  onRemove,
  onApply,
}: {
  text: string;
  chip: Chip;
  columns: Schema<"ColumnSchema">[];
  view: ViewName;
  scope?: FacetScope;
  onRemove: () => void;
  onApply: (chip: Chip) => void;
}) {
  const initial = draftFromChip(chip);
  const [open, setOpen] = useState(false);
  if (!initial) {
    return null;
  }
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <ChipShell text={text} onRemove={onRemove}>
        <PopoverTrigger asChild>
          <button type="button" className="focus-ring truncate rounded-sm px-1.5">
            {text}
          </button>
        </PopoverTrigger>
      </ChipShell>
      <PopoverContent align="start" className="w-72 gap-2 p-3">
        <ChipForm
          initial={initial}
          columns={columns}
          view={view}
          scope={scope}
          onApply={(next) => {
            onApply(next);
            setOpen(false);
          }}
        />
      </PopoverContent>
    </Popover>
  );
}

function ChipForm({
  initial,
  columns,
  view,
  scope,
  onApply,
}: {
  initial: Draft;
  columns: Schema<"ColumnSchema">[];
  view: ViewName;
  scope?: FacetScope;
  onApply: (chip: Chip) => void;
}) {
  const [draft, setDraft] = useState(initial);
  // The token being typed in a list. Apply adds it, so a typed but not entered token is not lost.
  const [typed, setTyped] = useState("");
  const kind = targetKind(draft.target, columns);
  const operators = chipOperators(kind);
  const op = operators.includes(draft.op) ? draft.op : (operators[0] ?? draft.op);
  const quoted = !["integer", "float", "timestamp", "boolean"].includes(kind);
  const listMode = isListOp(op);
  const tokens = listMode ? listTokens(op, draft.value) : [];
  const pending = typed.trim();
  const value =
    listMode && pending.length > 0 && !tokens.includes(pending)
      ? listValueText([...tokens, pending], quoted)
      : draft.value;
  const next = chipFromDraft({ ...draft, op, value }, kind);
  const emptyList = listMode && tokens.length === 0 && pending.length === 0;
  const names = columns.map((column) => column.name);
  if (!names.includes(draft.target) && !isCall(draft.target)) {
    names.unshift(draft.target);
  }
  const calls = view === "traces" || view === "spans" || view === "sessions";
  const fields = calls ? [...names, "score", "score_label", "has_score"] : names;
  const field = isCall(draft.target) ? null : fieldOf(draft.target, draft.name, kind);
  const facetView = facetViewOf(view);
  const select = field && facetView ? facetTarget(facetView, field) : undefined;
  const suggest: Suggest | undefined =
    field && facetView && select !== undefined && scope
      ? { view: facetView, field, scope }
      : undefined;
  return (
    <form
      className="flex flex-col gap-2"
      onSubmit={(event: FormEvent) => {
        event.preventDefault();
        if (next && !emptyList) {
          onApply(next);
        }
      }}
    >
      <span className="flex flex-col gap-1 text-caption">
        Field
        <FieldPicker
          value={draft.target}
          options={fields}
          onChange={(target) => {
            setTyped("");
            setDraft((current) => ({
              ...current,
              target,
              name: keepsName(current.target, target) ? current.name : "",
            }));
          }}
        />
      </span>
      {kind === "string_map" || isCall(draft.target) ? (
        <label className="flex flex-col gap-1 text-caption">
          {kind === "string_map" ? "Key" : "Score"}
          <input
            aria-label={kind === "string_map" ? "Key" : "Score"}
            className="h-7 rounded-md border border-input bg-transparent px-2"
            value={draft.name}
            onChange={(event) => {
              const name = event.target.value;
              setDraft((current) => ({ ...current, name }));
            }}
          />
        </label>
      ) : null}
      <label className="flex flex-col gap-1 text-caption">
        Operator
        <select
          aria-label="Operator"
          className="h-7 rounded-md border border-input bg-transparent px-1"
          value={op}
          onChange={(event) => {
            const chosen = event.target.value;
            setTyped("");
            setDraft((current) => ({ ...current, op: chosen }));
          }}
        >
          {operators.map((item) => (
            <option key={item} value={item}>
              {item}
            </option>
          ))}
        </select>
      </label>
      {needsValue(op) ? (
        <span className="flex flex-col gap-1 text-caption">
          Value
          <ValueInput
            op={op}
            value={draft.value}
            quoted={quoted}
            typed={typed}
            suggest={suggest}
            onChange={(input) => {
              setDraft((current) => ({ ...current, value: input }));
            }}
            onTyped={setTyped}
          />
        </span>
      ) : null}
      <Button type="submit" size="xs" disabled={next === null || emptyList}>
        Apply
      </Button>
    </form>
  );
}

function isCall(target: string): boolean {
  return target === "score" || target === "score_label" || target === "has_score";
}
