import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import { type KeyboardEvent, useEffect, useState } from "react";
import { facetQuery } from "@/api/queries";
import type { FacetScope } from "@/components/facets/facet-panel";
import { isListOp, listTokens, listValueText } from "@/components/filter-bar/logic";
import { useProject } from "@/components/shell/project-context";
import type { Field } from "@/lib/dsl/dsl";
import { type FacetView, fieldText } from "@/lib/facets";

const SUGGEST_DEBOUNCE_MS = 150;
const SUGGEST_LIMIT = 20;
/** The longest `q` the facets endpoint accepts, after trimming. */
const SUGGEST_MAX_QUERY = 200;

/** Where the suggestions of a value come from: a facet of the list's view (D7). */
export type Suggest = { view: FacetView; field: Field; scope: FacetScope };

/**
 * The value of a chip draft (D7). One text box, or for `IN`, `NOT IN` and `@>` a token list:
 * Enter or a suggestion adds a token, Backspace on an empty box removes the last one. With
 * `suggest`, the field's values from `/facets` show under the box.
 */
export function ValueInput({
  op,
  value,
  quoted,
  typed,
  suggest,
  onChange,
  onTyped,
}: {
  /** The draft's operator. */
  op: string;
  /** The draft's value text: one value, or the list text of `IN`, `NOT IN` and `@>`. */
  value: string;
  /** Whether list tokens are written as string literals (not for numbers and booleans). */
  quoted: boolean;
  /** The token being typed in a list; the parent adds it when the chip is applied (D7). */
  typed: string;
  suggest?: Suggest;
  onChange: (value: string) => void;
  onTyped: (text: string) => void;
}) {
  const list = isListOp(op);
  const items = list ? listTokens(op, value) : [];

  function addToken(token: string) {
    const next = token.trim();
    onTyped("");
    if (next.length === 0 || items.includes(next)) {
      return;
    }
    onChange(listValueText([...items, next], quoted));
  }

  function removeToken(index: number) {
    onChange(
      listValueText(
        items.filter((_, i) => i !== index),
        quoted,
      ),
    );
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.nativeEvent.isComposing) {
      return;
    }
    if (event.key === "Enter" && typed.trim().length > 0) {
      // A token list takes Enter as "add token". With nothing typed Enter submits the form.
      event.preventDefault();
      addToken(typed);
      return;
    }
    if (event.key === "Backspace" && typed.length === 0 && items.length > 0) {
      removeToken(items.length - 1);
    }
  }

  if (!list) {
    return (
      <div className="flex min-w-0 flex-col gap-1">
        <input
          aria-label="Value"
          className="h-7 rounded-md border border-input bg-transparent px-2 font-mono"
          value={value}
          onChange={(event) => onChange(event.target.value)}
        />
        {suggest ? (
          <Suggestions suggest={suggest} text={value} onPick={(next) => onChange(next)} />
        ) : null}
      </div>
    );
  }

  return (
    <div className="flex min-w-0 flex-col gap-1">
      <div className="flex min-h-7 flex-wrap items-center gap-1 rounded-md border border-input px-1 py-0.5">
        {items.map((item, index) => (
          <span
            key={item}
            className="inline-flex h-5 max-w-full items-center gap-0.5 rounded-sm bg-muted pl-1.5 font-mono text-caption"
          >
            <span className="truncate">{item}</span>
            <button
              type="button"
              aria-label={`Remove ${item}`}
              className="focus-ring rounded-sm px-0.5 text-muted-foreground"
              onClick={() => removeToken(index)}
            >
              <X className="size-3" />
            </button>
          </span>
        ))}
        <input
          aria-label="Value"
          className="h-5 min-w-20 flex-1 bg-transparent px-1 font-mono outline-none"
          placeholder={items.length === 0 ? "Value, then Enter" : undefined}
          value={typed}
          onChange={(event) => onTyped(event.target.value)}
          onKeyDown={onKeyDown}
        />
      </div>
      {suggest ? <Suggestions suggest={suggest} text={typed} onPick={addToken} /> : null}
    </div>
  );
}

/**
 * The values of the field that start from `text` (D7). The field and the text are debounced
 * together, so a map key typed letter by letter makes one request. Values of another field are
 * never shown: they are kept only for the field they were asked for.
 */
function Suggestions({
  suggest,
  text,
  onPick,
}: {
  suggest: Suggest;
  text: string;
  onPick: (value: string) => void;
}) {
  const { org, project } = useProject();
  const field = fieldText(suggest.field);
  const typed = text.trim();
  // Nothing is asked until the pair has been stable for the debounce time.
  const [asked, setAsked] = useState<{ field: string; q: string } | null>(null);
  useEffect(() => {
    const timer = window.setTimeout(() => setAsked({ field, q: typed }), SUGGEST_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [field, typed]);
  const query = useQuery({
    ...facetQuery(org, project, {
      view: suggest.view,
      field: asked?.field ?? "",
      from: suggest.scope.from,
      to: suggest.scope.to,
      includeEval: suggest.scope.includeEval,
      includeExperiments: suggest.scope.includeExperiments,
      search: suggest.scope.search,
      q: asked?.q || undefined,
      limit: SUGGEST_LIMIT,
    }),
    enabled: asked !== null && asked.q.length <= SUGGEST_MAX_QUERY,
  });
  const values = asked?.field === field ? (query.data?.values ?? []) : [];
  if (values.length === 0) {
    return null;
  }
  return (
    <ul
      aria-label="Suggested values"
      className="max-h-44 overflow-y-auto rounded-md border border-border"
    >
      {values.map((item) => (
        <li key={item.value}>
          <button
            type="button"
            className="focus-ring flex w-full items-center justify-between gap-2 px-2 py-1 text-left text-caption hover:bg-muted"
            onClick={() => onPick(item.value)}
          >
            <span className="truncate font-mono">{item.value}</span>
            <span className="shrink-0 text-muted-foreground">{item.count}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}
