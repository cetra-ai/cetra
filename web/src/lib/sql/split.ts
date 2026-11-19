import { isRecord } from "@/lib/sql/draft";
import { STORAGE_PREFIX } from "@/product";

export const SQL_SPLIT_KEY = `${STORAGE_PREFIX}sql.split`;

/** The editor's and the results' shares of the height, in percent. */
export type SqlSplit = { editor: number; results: number };

export const DEFAULT_SQL_SPLIT: SqlSplit = { editor: 40, results: 60 };

function isPercent(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0 && value < 100;
}

/** The stored split, or null when none is stored or the stored value is not a split. */
export function readSqlSplit(): SqlSplit | null {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(SQL_SPLIT_KEY) ?? "null");
    if (!isRecord(parsed)) {
      return null;
    }
    const { editor, results } = parsed;
    if (!isPercent(editor) || !isPercent(results)) {
      return null;
    }
    // Both shares come from one drag, so they add up to the whole height.
    if (Math.abs(editor + results - 100) > 1) {
      return null;
    }
    return { editor, results };
  } catch {
    return null;
  }
}

export function writeSqlSplit(layout: SqlSplit): void {
  try {
    localStorage.setItem(SQL_SPLIT_KEY, JSON.stringify(layout));
  } catch {
    // Private mode and quota failures leave the in-memory split in place.
  }
}
