export type DiffOp = "same" | "added" | "removed";

export interface DiffLine {
  op: DiffOp;
  text: string;
}

/** Above this many cells the LCS table is not built (a display guard, see `diffMiddle`). */
const MAX_TABLE_CELLS = 4_000_000;

function splitLines(text: string): string[] {
  return text === "" ? [] : text.split("\n");
}

/**
 * Line diff of two texts by longest common subsequence. Lines split on "\n"; "" is zero lines.
 * Within a changed block, removed lines come before added lines.
 */
export function diffLines(before: string, after: string): DiffLine[] {
  const a = splitLines(before);
  const b = splitLines(after);
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) {
    start++;
  }
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const out: DiffLine[] = [];
  for (const text of a.slice(0, start)) {
    out.push({ op: "same", text });
  }
  out.push(...diffMiddle(a.slice(start, endA), b.slice(start, endB)));
  for (const text of a.slice(endA)) {
    out.push({ op: "same", text });
  }
  return out;
}

/** The LCS walk of the lines between the common prefix and suffix. */
function diffMiddle(x: string[], y: string[]): DiffLine[] {
  const n = x.length;
  const m = y.length;
  if (n === 0 || m === 0 || n * m > MAX_TABLE_CELLS) {
    // A display guard against a huge table: every old line goes, then every new line.
    return [
      ...x.map((text): DiffLine => ({ op: "removed", text })),
      ...y.map((text): DiffLine => ({ op: "added", text })),
    ];
  }
  const width = m + 1;
  // table[i * width + j] is the LCS length of x[i..] and y[j..].
  const table = new Uint32Array((n + 1) * width);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      const cell =
        x[i] === y[j]
          ? (table[(i + 1) * width + j + 1] ?? 0) + 1
          : Math.max(table[(i + 1) * width + j] ?? 0, table[i * width + j + 1] ?? 0);
      table[i * width + j] = cell;
    }
  }
  const out: DiffLine[] = [];
  let removed: DiffLine[] = [];
  let added: DiffLine[] = [];
  const flush = () => {
    out.push(...removed, ...added);
    removed = [];
    added = [];
  };
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    const xi = x[i] ?? "";
    const yj = y[j] ?? "";
    if (xi === yj) {
      flush();
      out.push({ op: "same", text: xi });
      i++;
      j++;
    } else if ((table[(i + 1) * width + j] ?? 0) >= (table[i * width + j + 1] ?? 0)) {
      removed.push({ op: "removed", text: xi });
      i++;
    } else {
      added.push({ op: "added", text: yj });
      j++;
    }
  }
  for (; i < n; i++) {
    removed.push({ op: "removed", text: x[i] ?? "" });
  }
  for (; j < m; j++) {
    added.push({ op: "added", text: y[j] ?? "" });
  }
  flush();
  return out;
}
