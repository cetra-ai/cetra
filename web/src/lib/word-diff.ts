export type DiffPart = { kind: "same" | "added" | "removed"; text: string };

/** Above this many tokens on either side, `wordDiff` returns undefined (no diff shown). */
export const MAX_DIFF_WORDS = 2000;

/**
 * Splits into tokens: each word with the whitespace after it, plus a leading whitespace run as
 * its own token. Joining the tokens gives the text back.
 */
export function tokenize(text: string): string[] {
  return text.match(/^\s+|\S+\s*/g) ?? [];
}

type Op = { kind: DiffPart["kind"]; token: string };

function countWords(tokens: readonly string[]): number {
  let words = 0;
  for (const token of tokens) {
    if (token.trim().length > 0) {
      words += 1;
    }
  }
  return words;
}

/**
 * The parts that turn `before` into `after`: tokens in both (in LCS order) are "same", tokens
 * only in `before` "removed", tokens only in `after` "added". Adjacent parts of one kind are
 * merged. Removed parts come before added parts at each change. undefined when either side has
 * more than MAX_DIFF_WORDS non-whitespace tokens.
 */
export function wordDiff(before: string, after: string): DiffPart[] | undefined {
  const a = tokenize(before);
  const b = tokenize(after);
  if (countWords(a) > MAX_DIFF_WORDS || countWords(b) > MAX_DIFF_WORDS) {
    return undefined;
  }

  const ops: Op[] = [];
  let prefix = 0;
  while (prefix < a.length && prefix < b.length && a[prefix] === b[prefix]) {
    ops.push({ kind: "same", token: a[prefix] ?? "" });
    prefix += 1;
  }
  const limit = Math.min(a.length, b.length) - prefix;
  let suffix = 0;
  while (suffix < limit && a[a.length - 1 - suffix] === b[b.length - 1 - suffix]) {
    suffix += 1;
  }
  const middleA = a.slice(prefix, a.length - suffix);
  const middleB = b.slice(prefix, b.length - suffix);
  for (const op of lcsOps(middleA, middleB)) {
    ops.push(op);
  }
  for (let k = suffix; k > 0; k -= 1) {
    ops.push({ kind: "same", token: a[a.length - k] ?? "" });
  }

  return groupOps(ops);
}

/** The edit script of the middle parts, from a classic LCS table over suffixes (no recursion). */
function lcsOps(a: readonly string[], b: readonly string[]): Op[] {
  const n = a.length;
  const m = b.length;
  const width = m + 1;
  // lengths[i * width + j] is the LCS length of a[i..] and b[j..]. Fits 16 bits: at most 2,001 tokens.
  const lengths = new Uint16Array((n + 1) * width);
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      lengths[i * width + j] =
        a[i] === b[j]
          ? (lengths[(i + 1) * width + j + 1] ?? 0) + 1
          : Math.max(lengths[(i + 1) * width + j] ?? 0, lengths[i * width + j + 1] ?? 0);
    }
  }

  const ops: Op[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      ops.push({ kind: "same", token: a[i] ?? "" });
      i += 1;
      j += 1;
    } else if ((lengths[(i + 1) * width + j] ?? 0) >= (lengths[i * width + j + 1] ?? 0)) {
      ops.push({ kind: "removed", token: a[i] ?? "" });
      i += 1;
    } else {
      ops.push({ kind: "added", token: b[j] ?? "" });
      j += 1;
    }
  }
  for (; i < n; i += 1) {
    ops.push({ kind: "removed", token: a[i] ?? "" });
  }
  for (; j < m; j += 1) {
    ops.push({ kind: "added", token: b[j] ?? "" });
  }
  return ops;
}

/** Groups ops into parts: each change run is its removed text, then its added text. */
function groupOps(ops: readonly Op[]): DiffPart[] {
  const parts: DiffPart[] = [];
  const push = (kind: DiffPart["kind"], text: string) => {
    if (text === "") {
      return;
    }
    const last = parts[parts.length - 1];
    if (last && last.kind === kind) {
      last.text += text;
    } else {
      parts.push({ kind, text });
    }
  };

  let removed = "";
  let added = "";
  for (const op of ops) {
    if (op.kind === "same") {
      push("removed", removed);
      push("added", added);
      removed = "";
      added = "";
      push("same", op.token);
    } else if (op.kind === "removed") {
      removed += op.token;
    } else {
      added += op.token;
    }
  }
  push("removed", removed);
  push("added", added);
  return parts;
}
