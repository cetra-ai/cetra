import { CONTROL } from "./errors.js";

/** One result column: its name and its SQL type. */
export interface TableColumn {
  name: string;
  type: string;
}

/** Most characters a cell shows; a longer one keeps its first 59 characters and `…`. */
const CELL_MAX_CHARS = 60;

/** Characters, counted as code points (as Rust's `chars()` does). */
function length(text: string): number {
  return Array.from(text).length;
}

/** Every control character becomes one space; nothing else changes. */
function clean(text: string): string {
  return text.replace(CONTROL, " ");
}

function cut(text: string): string {
  const points = Array.from(text);
  if (points.length <= CELL_MAX_CHARS) return text;
  return `${points.slice(0, CELL_MAX_CHARS - 1).join("")}…`;
}

/** Cell text of one JSON value: `NULL` for null or a missing key, the raw text of a string,
 *  compact JSON for an array or object. */
function cellText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value);
}

/** A column is right-aligned when some row has a number in it and no row has a non-null value
 *  that is not a number. */
function isNumeric(rows: Record<string, unknown>[], name: string): boolean {
  let anyNumber = false;
  for (const row of rows) {
    const value = Object.hasOwn(row, name) ? row[name] : null;
    if (typeof value === "number") {
      anyNumber = true;
    } else if (value !== null && value !== undefined) {
      return false;
    }
  }
  return anyNumber;
}

/** An aligned table: a header line, a rule line, then one line per row, each ending in `\n`.
 *  Columns are two spaces apart, trailing spaces are trimmed, and no columns gives "". */
export function renderTable(columns: TableColumn[], rows: Record<string, unknown>[]): string {
  if (columns.length === 0) return "";
  const right = columns.map((column) => isNumeric(rows, column.name));
  const headers = columns.map((column) => cut(clean(column.name)));
  const cells = rows.map((row) =>
    columns.map((column) =>
      cut(clean(cellText(Object.hasOwn(row, column.name) ? row[column.name] : null))),
    ),
  );
  const widths = columns.map((_, index) => {
    let width = length(headers[index] ?? "");
    for (const row of cells) width = Math.max(width, length(row[index] ?? ""));
    return width;
  });
  const line = (values: string[]): string => {
    const text = values
      .map((value, index) => {
        const padding = " ".repeat(Math.max(0, (widths[index] ?? 0) - length(value)));
        return right[index] ? padding + value : value + padding;
      })
      .join("  ");
    return `${text.replace(/ +$/, "")}\n`;
  };
  const rules = widths.map((width) => "-".repeat(width));
  return [line(headers), line(rules), ...cells.map(line)].join("");
}
