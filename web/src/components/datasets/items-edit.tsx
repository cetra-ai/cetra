import { fieldErrors } from "@/api/problem";
import type { Schema } from "@/api/types";
import { ITEM_FIELDS, type ItemField } from "@/components/datasets/item-fields";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { Button } from "@/components/ui/button";
import { sameJson } from "@/lib/datasets";

/** The most items one batch edit or delete takes (D-J-23). */
export const BATCH_LIMIT = 1000;

type Item = Schema<"DatasetItem">;

/** The unsaved values of the items being edited, by item id. A key is present only for a changed field; `expected_output: null` clears it. */
export interface ItemDraft {
  input?: unknown;
  expected_output?: unknown;
  metadata?: Record<string, unknown>;
}

export type Drafts = Readonly<Record<string, ItemDraft>>;

/** Field errors of a batch save, by item id. */
export type CellErrors = Readonly<Record<string, Partial<Record<ItemField, string>>>>;

/** The item's own value of one field: an absent expected output is `undefined`. */
function itemValue(item: Item, field: ItemField): unknown {
  if (field === "input") {
    return item.input;
  }
  if (field === "expected_output") {
    return item.expected_output;
  }
  return item.metadata;
}

/** The value a cell shows: its draft when the draft has the field, else the item's own. */
export function draftedValue(item: Item, drafts: Drafts, field: ItemField): unknown {
  const draft = drafts[item.id];
  if (draft !== undefined && field in draft) {
    return draft[field];
  }
  return itemValue(item, field);
}

/** Whether the cell's field has a draft. */
export function hasDraft(drafts: Drafts, id: string, field: ItemField): boolean {
  const draft = drafts[id];
  return draft !== undefined && field in draft;
}

/**
 * The drafts with `field` of `item` set to `value`. A value equal to the item's own removes the
 * draft, and an item left without drafts leaves the map, so the map holds exactly the changes.
 */
export function withDraft(drafts: Drafts, item: Item, field: ItemField, value: unknown): Drafts {
  const draft: Record<string, unknown> = { ...drafts[item.id] };
  if (sameJson(value ?? null, itemValue(item, field) ?? null)) {
    delete draft[field];
  } else {
    draft[field] = value;
  }
  const next: Record<string, ItemDraft> = { ...drafts };
  if (Object.keys(draft).length === 0) {
    delete next[item.id];
  } else {
    next[item.id] = draft as ItemDraft;
  }
  return next;
}

/** The number of changed fields across the drafts: the count the bar shows. */
export function changedFields(drafts: Drafts): number {
  return Object.values(drafts).reduce((count, draft) => count + Object.keys(draft).length, 0);
}

/** The batch edit body for the drafts: each item with its changed fields only. */
export function editBody(drafts: Drafts): Schema<"EditItemsRequest"> {
  return {
    items: Object.entries(drafts).map(([id, draft]) => {
      const edit: Schema<"EditItem"> = { id };
      if ("input" in draft) {
        edit.input = draft.input;
      }
      if ("expected_output" in draft) {
        edit.expected_output = draft.expected_output ?? null;
      }
      if ("metadata" in draft) {
        edit.metadata = draft.metadata;
      }
      return edit;
    }),
  };
}

/**
 * Splits a batch save's field errors. `items[i].<field>` goes to the cell of request item `i`
 * (`ids[i]`) when the field is an item field; every other message is returned in `other`.
 */
export function editErrorsOf(
  caught: unknown,
  ids: readonly string[],
): { cells: CellErrors; other: string[] } {
  const cells: Record<string, Partial<Record<ItemField, string>>> = {};
  const other: string[] = [];
  for (const [field, message] of Object.entries(fieldErrors(caught))) {
    const match = /^items\[(\d+)\]\.(.+)$/s.exec(field);
    const id = match?.[1] !== undefined ? ids[Number(match[1])] : undefined;
    const name = match?.[2];
    const known =
      name === undefined
        ? undefined
        : ITEM_FIELDS.find(
            (candidate) =>
              name === candidate ||
              name.startsWith(`${candidate}.`) ||
              name.startsWith(`${candidate}[`),
          );
    if (id !== undefined && known !== undefined) {
      cells[id] = { ...cells[id], [known]: message };
    } else {
      other.push(message);
    }
  }
  return { cells, other };
}

/** The record without the given keys. */
export function omitKeys<T>(
  record: Readonly<Record<string, T>>,
  keys: readonly string[],
): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([key]) => !keys.includes(key)));
}

/** The bar under the toolbar while rows are checked: the count, Delete and Clear. */
export function SelectionBar({
  count,
  onDelete,
  onClear,
}: {
  count: number;
  onDelete: () => void;
  onClear: () => void;
}) {
  const reason =
    count > BATCH_LIMIT
      ? `Delete at most ${BATCH_LIMIT.toLocaleString("en-US")} items at a time`
      : undefined;
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-md border bg-muted/40 px-3 py-1 text-body">
      <span className="num">{`${count} selected`}</span>
      <DisabledReason reason={reason}>
        <Button
          type="button"
          variant="outline"
          size="xs"
          disabled={reason !== undefined}
          onClick={onDelete}
        >
          Delete
        </Button>
      </DisabledReason>
      <Button type="button" variant="ghost" size="xs" onClick={onClear}>
        Clear
      </Button>
    </div>
  );
}

/** The bar under the toolbar in edit mode: the unsaved changes, Discard and the save. */
export function EditBar({
  changes,
  items,
  saving,
  error,
  onDiscard,
  onSave,
}: {
  /** The changed fields. */
  changes: number;
  /** The items with a changed field: the batch's size. */
  items: number;
  saving: boolean;
  error?: string;
  onDiscard: () => void;
  onSave: () => void;
}) {
  const reason =
    items > BATCH_LIMIT
      ? `Save at most ${BATCH_LIMIT.toLocaleString("en-US")} items at a time`
      : undefined;
  const none = changes === 0;
  return (
    <div className="flex flex-col gap-1 rounded-md border bg-muted/40 px-3 py-1 text-body">
      <div className="flex flex-wrap items-center gap-2">
        <span className="num">
          {changes === 1 ? "1 unsaved change" : `${changes} unsaved changes`}
        </span>
        <Button
          type="button"
          variant="ghost"
          size="xs"
          disabled={none || saving}
          onClick={onDiscard}
        >
          Discard
        </Button>
        <DisabledReason reason={reason}>
          <Button
            type="button"
            size="xs"
            disabled={none || saving || reason !== undefined}
            onClick={onSave}
          >
            {saving ? "Saving…" : "Save as new version"}
          </Button>
        </DisabledReason>
      </div>
      <FormError message={error} />
    </div>
  );
}
