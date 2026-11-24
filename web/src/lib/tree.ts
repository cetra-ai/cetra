/** One span in a flat pre-order trace tree. */
export type TreeNode = {
  depth: number;
  span: { span_id: string };
};

/**
 * For each index, the index one past its last descendant.
 * A descendant run is the following spans with a greater depth.
 */
export function subtreeEnds(spans: readonly { depth: number }[]): Int32Array {
  const ends = new Int32Array(spans.length);
  const stack: number[] = [];
  for (let index = 0; index < spans.length; index++) {
    const span = spans[index];
    if (!span) {
      continue;
    }
    const depth = span.depth;
    while (stack.length > 0) {
      const parent = stack[stack.length - 1];
      const parentSpan = parent === undefined ? undefined : spans[parent];
      if (parent === undefined || !parentSpan || parentSpan.depth < depth) {
        break;
      }
      stack.pop();
      ends[parent] = index;
    }
    stack.push(index);
  }
  while (stack.length > 0) {
    const parent = stack.pop();
    if (parent !== undefined) {
      ends[parent] = spans.length;
    }
  }
  return ends;
}

export function hasChildren(ends: Int32Array, index: number): boolean {
  const end = ends[index];
  return end !== undefined && end > index + 1;
}

/**
 * Indices whose span matches `predicate`, plus their ancestors.
 * Each index is pushed and popped once; an ancestor already kept stops the walk.
 */
export function filterRows(
  spans: readonly { depth: number }[],
  ends: Int32Array,
  predicate: (index: number) => boolean,
): number[] {
  const keep = new Uint8Array(spans.length);
  const stack: number[] = [];
  const limit = Math.min(spans.length, ends.length);
  for (let index = 0; index < limit; index++) {
    const span = spans[index];
    if (!span) {
      continue;
    }
    while (stack.length > 0) {
      const parent = stack[stack.length - 1];
      const parentSpan = parent === undefined ? undefined : spans[parent];
      if (parent === undefined || !parentSpan || parentSpan.depth < span.depth) {
        break;
      }
      stack.pop();
    }
    if (predicate(index)) {
      keep[index] = 1;
      for (let cursor = stack.length - 1; cursor >= 0; cursor--) {
        const ancestor = stack[cursor];
        if (ancestor === undefined || keep[ancestor] === 1) {
          break;
        }
        keep[ancestor] = 1;
      }
    }
    stack.push(index);
  }
  const rows: number[] = [];
  for (let index = 0; index < spans.length; index++) {
    if (keep[index] === 1) {
      rows.push(index);
    }
  }
  return rows;
}

/** Indices to render, skipping descendants of collapsed span ids. */
export function visibleRows(
  spans: readonly TreeNode[],
  ends: Int32Array,
  collapsed: ReadonlySet<string>,
): number[] {
  const rows: number[] = [];
  let index = 0;
  while (index < spans.length) {
    const span = spans[index];
    if (!span) {
      break;
    }
    rows.push(index);
    const end = ends[index];
    index = collapsed.has(span.span.span_id) && end !== undefined ? end : index + 1;
  }
  return rows;
}

/**
 * Empty at 1 000 spans or fewer. Past that, every span at depth 2 or more
 * that has children starts collapsed, so the tree opens at depth 2.
 */
export function defaultCollapsed(spans: readonly TreeNode[]): Set<string> {
  if (spans.length <= 1000) {
    return new Set();
  }
  const ends = subtreeEnds(spans);
  const collapsed = new Set<string>();
  for (let index = 0; index < spans.length; index++) {
    const span = spans[index];
    if (span && span.depth >= 2 && hasChildren(ends, index)) {
      collapsed.add(span.span.span_id);
    }
  }
  return collapsed;
}

type LiveSpan = {
  span_id: string;
  parent_span_id?: string;
  start_time: string;
};

type LiveRow<Span extends LiveSpan> = {
  depth: number;
  detached: boolean;
  span: Span;
};

/**
 * Inserts one live span into a pre-order tree.
 * A known id is replaced in place. A known parent gets the span among its
 * children, ordered by start time then span id, after that child's descendants.
 * An unknown parent is appended as a detached root and is not moved later.
 */
export function insertSpan<Span extends LiveSpan>(
  spans: readonly LiveRow<Span>[],
  summary: Span,
): LiveRow<Span>[] {
  const existing = spans.findIndex((row) => row.span.span_id === summary.span_id);
  if (existing >= 0) {
    const current = spans[existing];
    if (!current) {
      return spans.slice();
    }
    const next = spans.slice();
    next[existing] = { ...current, span: summary };
    return next;
  }
  const parentIndex = summary.parent_span_id
    ? spans.findIndex((row) => row.span.span_id === summary.parent_span_id)
    : -1;
  const parent = parentIndex >= 0 ? spans[parentIndex] : undefined;
  if (!parent) {
    return [...spans, { depth: 0, detached: true, span: summary }];
  }
  const depth = parent.depth + 1;
  const ends = subtreeEnds(spans);
  const subtreeEnd = ends[parentIndex] ?? spans.length;
  let insertAt = subtreeEnd;
  let index = parentIndex + 1;
  while (index < subtreeEnd) {
    const row = spans[index];
    if (!row || row.depth !== depth) {
      index += 1;
      continue;
    }
    if (startsAfter(row.span, summary)) {
      insertAt = index;
      break;
    }
    const childEnd = ends[index] ?? index + 1;
    insertAt = childEnd;
    index = childEnd;
  }
  return [
    ...spans.slice(0, insertAt),
    { depth, detached: false, span: summary },
    ...spans.slice(insertAt),
  ];
}

/** Existing sorts after the incoming span: later start, or the same start and a greater id. */
function startsAfter(existing: LiveSpan, incoming: LiveSpan): boolean {
  if (existing.start_time !== incoming.start_time) {
    return existing.start_time > incoming.start_time;
  }
  return existing.span_id > incoming.span_id;
}

/** Ancestor indices from the root down to the parent, so a selection can expand its path. */
export function ancestorsOf(spans: readonly { depth: number }[], index: number): number[] {
  if (index < 0 || index >= spans.length) {
    return [];
  }
  const current = spans[index];
  if (!current) {
    return [];
  }
  const ancestors: number[] = [];
  let depth = current.depth;
  for (let cursor = index - 1; cursor >= 0 && depth > 0; cursor--) {
    const earlier = spans[cursor];
    if (earlier && earlier.depth < depth) {
      ancestors.push(cursor);
      depth = earlier.depth;
    }
  }
  ancestors.reverse();
  return ancestors;
}

// Rough widths of a name cell's parts at the grid's font size.
const NAME_CHAR_PX = 7;
// The toggle gutter, the kind icon, a gap and the right padding.
const NAME_BASE_PX = 20 + 14 + 4 + 8;
const NAME_INDENT_PX = 16;
const NAME_ERROR_DOT_PX = 12;
const NAME_DETACHED_BADGE_PX = 64;
export const NAME_AUTO_MIN_PX = 220;
export const NAME_AUTO_MAX_PX = 360;

/** The Name column width that fits the longest visible name, kept between 220px and 360px. */
export function autoNameWidth(
  spans: readonly {
    depth: number;
    detached?: boolean;
    span: { name: string; status: string };
  }[],
  visible: readonly number[],
): number {
  let widest = 0;
  for (const index of visible) {
    const row = spans[index];
    if (!row) {
      continue;
    }
    const width =
      NAME_BASE_PX +
      row.depth * NAME_INDENT_PX +
      row.span.name.length * NAME_CHAR_PX +
      (row.span.status === "error" ? NAME_ERROR_DOT_PX : 0) +
      (row.detached ? NAME_DETACHED_BADGE_PX : 0);
    widest = Math.max(widest, width);
  }
  return Math.min(NAME_AUTO_MAX_PX, Math.max(NAME_AUTO_MIN_PX, Math.round(widest)));
}

/** The connector lines drawn in one row's indent. */
export type TreeGuide = {
  /** The elbow to this row: "mid" (├, later siblings follow), "last" (└), none for a root. */
  elbow: "mid" | "last" | null;
  /**
   * One entry per ancestor level from depth 1 to the parent: whether that ancestor has later
   * siblings, so its vertical line runs through this row.
   */
  through: boolean[];
  /** The row's own children follow it, so a line starts under its icon. */
  down: boolean;
};

/**
 * The connector lines of the rows shown, from their depths in pre-order.
 * A line belongs to the rows actually shown, so a filtered or collapsed tree stops it at
 * the last shown child.
 */
export function treeGuides(depths: readonly number[]): TreeGuide[] {
  const guides: TreeGuide[] = new Array(depths.length);
  // later[d]: a row of depth d follows, before any shallower row.
  const later: boolean[] = [];
  for (let index = depths.length - 1; index >= 0; index--) {
    const depth = depths[index] ?? 0;
    const next = depths[index + 1];
    const through: boolean[] = [];
    for (let level = 1; level < depth; level++) {
      through.push(later[level] === true);
    }
    guides[index] = {
      elbow: depth === 0 ? null : later[depth] ? "mid" : "last",
      through,
      down: next !== undefined && next > depth,
    };
    later[depth] = true;
    later.length = depth + 1;
  }
  return guides;
}
