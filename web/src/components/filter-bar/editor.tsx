import {
  autocompletion,
  type Completion,
  type CompletionContext,
  type CompletionResult,
} from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { sql } from "@codemirror/lang-sql";
import { EditorState, StateField, type Transaction } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  keymap,
  placeholder as placeholderExtension,
} from "@codemirror/view";
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { facetQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import type { FacetScope } from "@/components/facets/facet-panel";
import {
  columnMap,
  completeFilter,
  facetTarget,
  facetViewOf,
  valueSlot,
} from "@/components/filter-bar/logic";
import { useProject } from "@/components/shell/project-context";
import { quoteString } from "@/lib/dsl/dsl";
import { fieldText } from "@/lib/facets";

/** The pause after the last keystroke before a value request (D7). */
const VALUE_DEBOUNCE_MS = 150;
const VALUE_LIMIT = 20;

const errorMark = Decoration.mark({ class: "cm-filter-error" });

/** jsdom ranges have no layout rects. Browsers already implement these. */
export function ensureLayoutRects() {
  const prototypes = [Range.prototype, Element.prototype];
  for (const prototype of prototypes) {
    if (typeof prototype.getClientRects !== "function") {
      prototype.getClientRects = () => {
        const list = [] as unknown as DOMRectList;
        list.item = () => null;
        return list;
      };
    }
    if (typeof prototype.getBoundingClientRect !== "function") {
      prototype.getBoundingClientRect = () => new DOMRect();
    }
  }
}

const editorTheme = EditorView.theme({
  "&": {
    fontSize: "12px",
    backgroundColor: "transparent",
  },
  "&.cm-focused": {
    outline: "2px solid var(--ring)",
    outlineOffset: "1px",
  },
  ".cm-scroller": {
    fontFamily: "var(--font-mono), ui-monospace, monospace",
    overflowX: "auto",
    overflowY: "hidden",
    lineHeight: "24px",
  },
  ".cm-content": {
    padding: "0",
    caretColor: "var(--foreground)",
  },
  ".cm-line": {
    padding: "0",
  },
  ".cm-tooltip.cm-tooltip-autocomplete": {
    fontSize: "12px",
  },
  ".cm-filter-error": {
    textDecoration: "underline wavy var(--destructive)",
  },
});

export function FilterEditor({
  value,
  selection,
  errorRange,
  viewName,
  schema,
  scope,
  placeholder = "Filter, e.g. status = 'error' AND cost_total > 0.1",
  onApply,
  onCancel,
}: {
  value: string;
  selection?: { from: number; to: number };
  errorRange: { from: number; to: number } | null;
  viewName: Schema<"ViewName">;
  schema: Schema<"SchemaResponse"> | undefined;
  /** The list's window and flags: value suggestions are fetched with it (D7). */
  scope?: FacetScope;
  placeholder?: string;
  onApply: (text: string) => void;
  onCancel: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const parent = useRef<HTMLDivElement>(null);
  const onApplyRef = useRef(onApply);
  const onCancelRef = useRef(onCancel);
  const appliedRef = useRef(value);
  const schemaRef = useRef(schema);
  const viewRef = useRef(viewName);
  const valuesRef = useRef<ValueSource>({ org, project, queryClient, scope });
  onApplyRef.current = onApply;
  onCancelRef.current = onCancel;
  appliedRef.current = value;
  schemaRef.current = schema;
  viewRef.current = viewName;
  valuesRef.current = { org, project, queryClient, scope };

  const selectFrom = selection?.from;
  const selectTo = selection?.to;
  const errorFrom = errorRange?.from;
  const errorTo = errorRange?.to;

  useEffect(() => {
    const host = parent.current;
    if (!host) {
      return;
    }
    ensureLayoutRects();
    const selected =
      selectFrom !== undefined && selectTo !== undefined
        ? { anchor: selectFrom, head: selectTo }
        : undefined;
    const range =
      errorFrom !== undefined && errorTo !== undefined ? { from: errorFrom, to: errorTo } : null;
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: value,
        ...(selected ? { selection: selected } : {}),
        extensions: [
          editorTheme,
          sql(),
          placeholderExtension(placeholder),
          history(),
          singleLine(),
          errorField(range),
          EditorView.contentAttributes.of({
            "aria-label": "Filter",
            "aria-invalid": range ? "true" : "false",
          }),
          keymap.of([
            {
              key: "Enter",
              run: (editor) => {
                onApplyRef.current(editor.state.doc.toString());
                return true;
              },
            },
            {
              key: "Escape",
              run: (editor) => {
                const applied = appliedRef.current;
                editor.dispatch({
                  changes: { from: 0, to: editor.state.doc.length, insert: applied },
                  selection: { anchor: applied.length },
                });
                editor.contentDOM.blur();
                onCancelRef.current();
                return true;
              },
            },
            ...historyKeymap,
            ...defaultKeymap.filter((binding) => binding.key !== "Enter"),
          ]),
          autocompletion({
            override: [
              (context: CompletionContext) =>
                completeValue(context, viewRef.current, schemaRef.current, valuesRef.current),
              (context: CompletionContext) =>
                completeFilter(context, viewRef.current, schemaRef.current),
            ],
          }),
        ],
      }),
    });
    view.focus();
    return () => {
      view.destroy();
    };
  }, [value, selectFrom, selectTo, errorFrom, errorTo, placeholder]);

  return <div ref={parent} className="min-w-0 flex-1" />;
}

type ValueSource = {
  org: string;
  project: string;
  queryClient: QueryClient;
  scope: FacetScope | undefined;
};

/**
 * Values of the field in a value position (D7), from `/facets` with the list's window and the
 * text typed so far as `q`. Waits for a pause in typing first; answers come from the query cache.
 */
async function completeValue(
  context: CompletionContext,
  view: Schema<"ViewName">,
  schema: Schema<"SchemaResponse"> | undefined,
  source: ValueSource,
): Promise<CompletionResult | null> {
  const facetView = facetViewOf(view);
  const found = valueSlot(context.state.doc.toString(), context.pos, view, columnMap(schema, view));
  const { scope } = source;
  if (!found || !facetView || !scope) {
    return null;
  }
  const select = facetTarget(facetView, found.field);
  const typed = found.prefix.trim();
  if (select === undefined || typed.length > 200) {
    return null;
  }
  await new Promise((resolve) => window.setTimeout(resolve, VALUE_DEBOUNCE_MS));
  if (context.aborted) {
    return null;
  }
  try {
    const answer = await source.queryClient.fetchQuery(
      facetQuery(source.org, source.project, {
        view: facetView,
        field: fieldText(found.field),
        from: scope.from,
        to: scope.to,
        includeEval: scope.includeEval,
        includeExperiments: scope.includeExperiments,
        search: scope.search,
        q: typed || undefined,
        limit: VALUE_LIMIT,
      }),
    );
    if (context.aborted) {
      return null;
    }
    const options: Completion[] = (answer.values ?? []).map((item) => ({
      label:
        select === "bool" ? (item.value === "true" ? "TRUE" : "FALSE") : quoteString(item.value),
      detail: String(item.count),
    }));
    // Filtering would hide `'error'` for the typed `e` (the server already matched `q`).
    return options.length === 0 ? null : { from: found.from, to: found.to, options, filter: false };
  } catch {
    return null;
  }
}

function singleLine() {
  return EditorState.transactionFilter.of((tr: Transaction) => {
    if (!tr.docChanged) {
      return tr;
    }
    let newline = false;
    const changes: { from: number; to: number; insert: string }[] = [];
    tr.changes.iterChanges((from, to, _fromB, _toB, inserted) => {
      const text = inserted.toString();
      if (text.includes("\n") || text.includes("\r")) {
        newline = true;
      }
      changes.push({ from, to, insert: text.replace(/[\n\r]/g, " ") });
    });
    if (!newline) {
      return tr;
    }
    return { changes, selection: tr.selection };
  });
}

function errorField(range: { from: number; to: number } | null) {
  return StateField.define<DecorationSet>({
    create(state) {
      if (!range) {
        return Decoration.none;
      }
      const from = Math.min(range.from, state.doc.length);
      const to = Math.min(Math.max(range.to, from), state.doc.length);
      if (from === to) {
        return Decoration.none;
      }
      return Decoration.set([errorMark.range(from, to)]);
    },
    update(marks, tr) {
      return marks.map(tr.changes);
    },
    provide: (field) => EditorView.decorations.from(field),
  });
}
