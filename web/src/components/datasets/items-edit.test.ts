import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import itemsFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items.json";
import {
  changedFields,
  type Drafts,
  draftedValue,
  editBody,
  editErrorsOf,
  hasDraft,
  omitKeys,
  withDraft,
} from "./items-edit";

const ITEM = itemsFixture.data[0] as unknown as Schema<"DatasetItem">;
const OTHER = { ...ITEM, id: "01900000-0000-7000-8000-000000000d09" };

/** The parsed problem the API client throws for a 400 with field errors. */
function problem(errors: { field: string; message: string }[]) {
  return {
    type: "about:blank",
    title: "Bad Request",
    status: 400,
    code: "validation_failed",
    errors,
  };
}

describe("withDraft", () => {
  it("keeps a value that differs from the item's own", () => {
    const drafts = withDraft({}, ITEM, "metadata", { topic: "billing" });

    expect(drafts).toEqual({ [ITEM.id]: { metadata: { topic: "billing" } } });
    expect(hasDraft(drafts, ITEM.id, "metadata")).toBe(true);
    expect(hasDraft(drafts, ITEM.id, "input")).toBe(false);
  });

  it("drops the draft when the value is the item's own again", () => {
    const edited = withDraft({}, ITEM, "metadata", { topic: "billing" });

    expect(withDraft(edited, ITEM, "metadata", ITEM.metadata)).toEqual({});
  });

  it("keeps the other fields of an item when one is restored", () => {
    const edited = withDraft(withDraft({}, ITEM, "input", { q: 1 }), ITEM, "metadata", {});

    const restored = withDraft(edited, ITEM, "metadata", ITEM.metadata);

    expect(restored).toEqual({ [ITEM.id]: { input: { q: 1 } } });
  });

  it("treats an absent expected output and a cleared one (null) as the same", () => {
    const bare = { ...ITEM, expected_output: undefined };

    expect(withDraft({}, bare, "expected_output", null)).toEqual({});
  });
});

describe("drafted values and counts", () => {
  it("shows the draft when there is one, else the item's value", () => {
    const drafts: Drafts = { [ITEM.id]: { input: "new" } };

    expect(draftedValue(ITEM, drafts, "input")).toBe("new");
    expect(draftedValue(ITEM, drafts, "metadata")).toEqual(ITEM.metadata);
    expect(draftedValue(OTHER, drafts, "input")).toEqual(OTHER.input);
  });

  it("counts the changed fields across items", () => {
    const drafts: Drafts = {
      [ITEM.id]: { input: 1, metadata: {} },
      [OTHER.id]: { expected_output: null },
    };

    expect(changedFields(drafts)).toBe(3);
    expect(changedFields({})).toBe(0);
  });
});

describe("editBody", () => {
  it("sends each item with its changed fields only, and a cleared expected output as null", () => {
    const drafts: Drafts = {
      [ITEM.id]: { metadata: { topic: "billing" } },
      [OTHER.id]: { expected_output: null, input: "x" },
    };

    expect(editBody(drafts)).toEqual({
      items: [
        { id: ITEM.id, metadata: { topic: "billing" } },
        { id: OTHER.id, input: "x", expected_output: null },
      ],
    });
  });
});

describe("editErrorsOf", () => {
  it("places an item field error on the cell of its request index", () => {
    const caught = problem([{ field: "items[1].input", message: "input is required" }]);

    expect(editErrorsOf(caught, [ITEM.id, OTHER.id])).toEqual({
      cells: { [OTHER.id]: { input: "input is required" } },
      other: [],
    });
  });

  it("keeps errors that do not belong to a cell as messages", () => {
    const caught = problem([
      { field: "items[0].id", message: "no such item" },
      { field: "items", message: "too many items" },
    ]);

    expect(editErrorsOf(caught, [ITEM.id])).toEqual({
      cells: {},
      other: ["no such item", "too many items"],
    });
  });

  it("reads a nested field of an item as the item field", () => {
    const caught = problem([{ field: "items[0].metadata.topic", message: "must be a string" }]);

    expect(editErrorsOf(caught, [ITEM.id]).cells).toEqual({
      [ITEM.id]: { metadata: "must be a string" },
    });
  });
});

describe("omitKeys", () => {
  it("returns the record without the given keys", () => {
    expect(omitKeys({ a: 1, b: 2, c: 3 }, ["a", "c"])).toEqual({ b: 2 });
  });
});
