/** Only the first rows are measured, so a wide result does not stall the render. */
export const FIT_ROWS = 200;

const MIN_WIDTH = 80;
const MAX_WIDTH = 480;
const CHAR_PX = 7;
const PADDING_PX = 32;

/** Display text of one JSON cell. `null` and a missing key read `NULL`; objects are JSON. */
export function formatSqlCell(value: unknown): string {
  if (value === null || value === undefined) {
    return "NULL";
  }
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return JSON.stringify(value);
}

/**
 * Pixel width that fits the header and the first FIT_ROWS display values, clamped to
 * MIN_WIDTH..MAX_WIDTH.
 */
export function fitWidth(header: string, values: readonly unknown[]): number {
  let maxChars = header.length;
  const limit = Math.min(values.length, FIT_ROWS);
  for (let i = 0; i < limit; i += 1) {
    maxChars = Math.max(maxChars, formatSqlCell(values[i]).length);
  }
  return Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, maxChars * CHAR_PX + PADDING_PX));
}
