import { expect, it } from "vitest";
import {
  columnChoiceKey,
  lockedColumnIds,
  moveColumn,
  readColumnChoice,
  reorderVisible,
  visibleColumnIds,
  writeColumnChoice,
} from "@/components/data-table/column-layout";

const specs = [
  { id: "status", locked: true, defaultVisible: true },
  { id: "start", defaultVisible: true },
  { id: "name", locked: true, defaultVisible: true },
  { id: "cost", defaultVisible: true },
  { id: "score:helpfulness", defaultVisible: true },
  { id: "env", defaultVisible: true },
  { id: "release" },
];

it("uses defaults, with score columns where the specs put them", () => {
  expect(visibleColumnIds(specs, undefined).map((id) => id)).toEqual([
    "status",
    "start",
    "name",
    "cost",
    "score:helpfulness",
    "env",
  ]);
});

it("restores visibility and order and ignores an unknown id", () => {
  expect(visibleColumnIds(specs, ["nope", "env", "cost", "score:missing"])).toEqual([
    "status",
    "name",
    "env",
    "cost",
  ]);
});

it("reorders a visible id and refuses the ends", () => {
  expect(reorderVisible(["status", "name", "cost"], "cost", -1)).toEqual([
    "status",
    "cost",
    "name",
  ]);
  expect(reorderVisible(["status", "name"], "status", -1)).toBeUndefined();
});

it("reads and writes the table's column choice", () => {
  expect(readColumnChoice("traces")).toBeUndefined();
  writeColumnChoice("traces", ["env", "name"]);
  expect(localStorage.getItem(columnChoiceKey("traces"))).toBe("env,name");
  expect(readColumnChoice("traces")).toEqual(["env", "name"]);
  writeColumnChoice("traces", undefined);
  expect(localStorage.getItem(columnChoiceKey("traces"))).toBeNull();
});

it("collects the locked ids", () => {
  expect([...lockedColumnIds(specs)]).toEqual(["status", "name"]);
});

const ids = ["a", "b", "c", "d"];
const none: ReadonlySet<string> = new Set();

it("moves an unlocked column right and left, clamping the index to the array", () => {
  expect(moveColumn(ids, "a", 2, none)).toEqual(["b", "c", "a", "d"]);
  expect(moveColumn(ids, "d", 0, none)).toEqual(["d", "a", "b", "c"]);
  expect(moveColumn(ids, "a", 99, none)).toEqual(["b", "c", "d", "a"]);
  expect(moveColumn(ids, "b", -5, none)).toEqual(["b", "a", "c", "d"]);
});

it("never moves a locked id", () => {
  expect(moveColumn(ids, "a", 2, new Set(["a"]))).toBeUndefined();
});

it("never lands a column before the leading locked ids", () => {
  const locked = new Set(["a"]);
  expect(moveColumn(ids, "c", 0, locked)).toEqual(["a", "c", "b", "d"]);
  expect(moveColumn(ids, "d", 1, new Set(["a", "b"]))).toEqual(["a", "b", "d", "c"]);
});

it("returns undefined when the order would not change or the id is unknown", () => {
  expect(moveColumn(ids, "b", 1, none)).toBeUndefined();
  expect(moveColumn(ids, "a", -3, new Set(["b"]))).toBeUndefined();
  expect(moveColumn(ids, "zzz", 0, none)).toBeUndefined();
});
