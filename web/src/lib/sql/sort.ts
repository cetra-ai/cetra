import { isNumericType, isTextType } from "@/lib/sql/types";

/** How a column's cells compare, from its Arrow type. */
export type SqlSortKind = "number" | "text" | "other";

export function sortKindOf(type: string): SqlSortKind {
  if (isNumericType(type)) {
    return "number";
  }
  if (isTextType(type)) {
    return "text";
  }
  return "other";
}

/** A number, a numeric string (Decimal may arrive as one), or undefined when it does not parse. */
function numberOf(value: unknown): number | undefined {
  if (typeof value === "number") {
    return Number.isNaN(value) ? undefined : value;
  }
  if (typeof value === "string" && value.trim() !== "") {
    const parsed = Number(value);
    return Number.isNaN(parsed) ? undefined : parsed;
  }
  return undefined;
}

/** The text a "text" cell compares by. */
function textOf(value: unknown): string | undefined {
  if (value === null || value === undefined) {
    return undefined;
  }
  return typeof value === "string" ? value : JSON.stringify(value);
}

/** The key an "other" cell compares by: booleans as booleans, everything else as text. */
function otherOf(value: unknown): string | boolean | undefined {
  if (value === null || value === undefined) {
    return undefined;
  }
  if (typeof value === "boolean" || typeof value === "string") {
    return value;
  }
  return JSON.stringify(value);
}

function compareOther(a: string | boolean, b: string | boolean): number {
  if (typeof a === "boolean" && typeof b === "boolean") {
    return Number(a) - Number(b);
  }
  const x = String(a);
  const y = String(b);
  return x < y ? -1 : x > y ? 1 : 0;
}

/** Compares two keys; a missing key sorts after every present one. */
function compareKeys<T>(a: T | undefined, b: T | undefined, compare: (x: T, y: T) => number) {
  if (a === undefined || b === undefined) {
    return a === b ? 0 : a === undefined ? 1 : -1;
  }
  return compare(a, b);
}

/** True when a cell sorts with the nulls: null, missing, or unparseable in a number column. */
export function isMissingSqlValue(value: unknown, kind: SqlSortKind): boolean {
  if (value === null || value === undefined) {
    return true;
  }
  return kind === "number" && numberOf(value) === undefined;
}

/**
 * Ascending comparison of two cells. Missing cells (see `isMissingSqlValue`) sort after
 * every value, and two missing cells compare equal.
 */
export function compareSqlValues(a: unknown, b: unknown, kind: SqlSortKind): number {
  if (kind === "number") {
    return compareKeys(numberOf(a), numberOf(b), (x, y) => (x < y ? -1 : x > y ? 1 : 0));
  }
  if (kind === "text") {
    return compareKeys(textOf(a), textOf(b), (x, y) => x.localeCompare(y));
  }
  return compareKeys(otherOf(a), otherOf(b), compareOther);
}

/**
 * Sorts rows by one column. Missing cells stay last in both directions. Equal cells keep
 * their original order (`index` ascending), so a descending sort is still stable.
 */
export function sortSqlRows<T extends { index: number }>(
  rows: readonly T[],
  cellOf: (row: T) => unknown,
  kind: SqlSortKind,
  desc: boolean,
): T[] {
  return [...rows].sort((a, b) => {
    const x = cellOf(a);
    const y = cellOf(b);
    let order = compareSqlValues(x, y, kind);
    if (desc && !isMissingSqlValue(x, kind) && !isMissingSqlValue(y, kind)) {
      order = -order;
    }
    return order || a.index - b.index;
  });
}
