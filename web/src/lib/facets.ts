import {
  appendFilter,
  type Chip,
  type ChipValue,
  type CompareOp,
  type Conjunct,
  type Field,
  parseChip,
  serializeChip,
  serializeField,
  splitConjuncts,
  tokenize,
} from "@/lib/dsl/dsl";

/** The lists that have facets. `users` counts traces, so its facets use the traces view. */
export type FacetView = "traces" | "spans" | "sessions";

/** The section kinds of the facet panel (D3). `values` is a scalar field, `list` a string list. */
export type FacetKind = "values" | "list" | "bool" | "keys" | "text" | "range";

export type FacetDef = {
  id: string;
  field: Field;
  label: string;
  kind: FacetKind;
  defaultOpen?: boolean;
  unit?: "duration_ms" | "usd" | "count";
};

/** The facet sections of each list, in display order (D3). */
export const FACETS: Record<FacetView, FacetDef[]> = {
  traces: [
    {
      id: "status",
      field: { column: "status" },
      label: "Status",
      kind: "values",
      defaultOpen: true,
    },
    { id: "name", field: { column: "name" }, label: "Name", kind: "values", defaultOpen: true },
    {
      id: "environment",
      field: { column: "environment" },
      label: "Environment",
      kind: "values",
      defaultOpen: true,
    },
    { id: "models", field: { column: "models" }, label: "Models", kind: "list" },
    { id: "tags", field: { column: "tags" }, label: "Tags", kind: "list" },
    { id: "release", field: { column: "release" }, label: "Release", kind: "values" },
    { id: "user_id", field: { column: "user_id" }, label: "User ID", kind: "values" },
    { id: "session_id", field: { column: "session_id" }, label: "Session ID", kind: "values" },
    { id: "trace_id", field: { column: "trace_id" }, label: "Trace ID", kind: "text" },
    { id: "has_root", field: { column: "has_root" }, label: "Has root", kind: "bool" },
    { id: "metadata", field: { column: "metadata" }, label: "Metadata", kind: "keys" },
    {
      id: "duration_ms",
      field: { column: "duration_ms" },
      label: "Duration",
      kind: "range",
      defaultOpen: true,
      unit: "duration_ms",
    },
    {
      id: "cost_total",
      field: { column: "cost_total" },
      label: "Cost",
      kind: "range",
      unit: "usd",
    },
    {
      id: "total_tokens",
      field: { column: "total_tokens" },
      label: "Tokens",
      kind: "range",
      unit: "count",
    },
    {
      id: "span_count",
      field: { column: "span_count" },
      label: "Spans",
      kind: "range",
      unit: "count",
    },
    {
      id: "error_count",
      field: { column: "error_count" },
      label: "Errors",
      kind: "range",
      unit: "count",
    },
    {
      id: "llm_count",
      field: { column: "llm_count" },
      label: "LLM calls",
      kind: "range",
      unit: "count",
    },
  ],
  spans: [
    {
      id: "span_kind",
      field: { column: "span_kind" },
      label: "Kind",
      kind: "values",
      defaultOpen: true,
    },
    {
      id: "status",
      field: { column: "status" },
      label: "Status",
      kind: "values",
      defaultOpen: true,
    },
    { id: "name", field: { column: "name" }, label: "Name", kind: "values", defaultOpen: true },
    { id: "model", field: { column: "model" }, label: "Model", kind: "values" },
    { id: "provider", field: { column: "provider" }, label: "Provider", kind: "values" },
    { id: "service_name", field: { column: "service_name" }, label: "Service", kind: "values" },
    { id: "operation", field: { column: "operation" }, label: "Operation", kind: "values" },
    { id: "environment", field: { column: "environment" }, label: "Environment", kind: "values" },
    { id: "release", field: { column: "release" }, label: "Release", kind: "values" },
    { id: "tags", field: { column: "tags" }, label: "Tags", kind: "list" },
    { id: "user_id", field: { column: "user_id" }, label: "User ID", kind: "values" },
    { id: "session_id", field: { column: "session_id" }, label: "Session ID", kind: "values" },
    { id: "trace_id", field: { column: "trace_id" }, label: "Trace ID", kind: "text" },
    { id: "metadata", field: { column: "metadata" }, label: "Metadata", kind: "keys" },
    { id: "attributes", field: { column: "attributes" }, label: "Attributes", kind: "keys" },
    {
      id: "duration_ms",
      field: { column: "duration_ms" },
      label: "Duration",
      kind: "range",
      defaultOpen: true,
      unit: "duration_ms",
    },
    {
      id: "cost_total",
      field: { column: "cost_total" },
      label: "Cost",
      kind: "range",
      unit: "usd",
    },
    {
      id: "total_tokens",
      field: { column: "total_tokens" },
      label: "Tokens",
      kind: "range",
      unit: "count",
    },
  ],
  sessions: [
    {
      id: "user_id",
      field: { column: "user_id" },
      label: "User ID",
      kind: "values",
      defaultOpen: true,
    },
    { id: "tags", field: { column: "tags" }, label: "Tags", kind: "list", defaultOpen: true },
    {
      id: "duration_ms",
      field: { column: "duration_ms" },
      label: "Duration",
      kind: "range",
      defaultOpen: true,
      unit: "duration_ms",
    },
    {
      id: "trace_count",
      field: { column: "trace_count" },
      label: "Traces",
      kind: "range",
      unit: "count",
    },
    {
      id: "error_count",
      field: { column: "error_count" },
      label: "Errors",
      kind: "range",
      unit: "count",
    },
    {
      id: "cost_total",
      field: { column: "cost_total" },
      label: "Cost",
      kind: "range",
      unit: "usd",
    },
    {
      id: "total_tokens",
      field: { column: "total_tokens" },
      label: "Tokens",
      kind: "range",
      unit: "count",
    },
  ],
};

/** The DSL text of a field: `name` or `metadata['k']`. */
export function fieldText(field: Field): string {
  return serializeField(field);
}

/** A selection of a values facet (D4). `none` is the "(none)" row: rows with no value. */
export type Selection = { mode: "include" | "exclude"; values: string[]; none: boolean };

/** The kinds whose selection is written as `=`, `IN`, `IS NULL` (or their negations). */
export type SelectKind = "values" | "bool" | "list";

export type TextOp = "equals" | "contains" | "starts" | "not_contains";

const EQUALITY_OPS: ReadonlySet<CompareOp> = new Set(["=", "!="]);
const LIKE_OPS: ReadonlySet<CompareOp> = new Set(["LIKE", "ILIKE", "NOT LIKE", "NOT ILIKE"]);
const LOWER_OPS: ReadonlySet<CompareOp> = new Set([">=", ">"]);
const UPPER_OPS: ReadonlySet<CompareOp> = new Set(["<", "<="]);
const RANGE_OPS: ReadonlySet<CompareOp> = new Set(["<", "<=", ">", ">="]);

type Parsed = { conjunct: Conjunct; chip: Chip };

function parsedConjuncts(filter: string): Parsed[] {
  return splitConjuncts(filter).map((conjunct) => ({ conjunct, chip: parseChip(conjunct.text) }));
}

/** Same column and, for map fields, the same key (D4: `Field` equality). */
export function sameField(left: Field, right: Field): boolean {
  if (left.column !== right.column) {
    return false;
  }
  if ("key" in left || "key" in right) {
    return "key" in left && "key" in right && left.key === right.key;
  }
  return true;
}

function chipField(chip: Chip): Field | undefined {
  return chip.kind === "compare" ||
    chip.kind === "null" ||
    chip.kind === "in" ||
    chip.kind === "contains"
    ? chip.field
    : undefined;
}

/** Whether a parsed conjunct is one the facet of `kind` on `field` owns (D4). */
function owns(chip: Chip, field: Field, kind: FacetKind): boolean {
  const own = chipField(chip);
  if (own === undefined || !sameField(own, field)) {
    return false;
  }
  switch (kind) {
    case "values":
      return (
        chip.kind === "in" ||
        chip.kind === "null" ||
        (chip.kind === "compare" && (EQUALITY_OPS.has(chip.op) || LIKE_OPS.has(chip.op)))
      );
    case "bool":
      return (
        chip.kind === "in" ||
        chip.kind === "null" ||
        (chip.kind === "compare" && EQUALITY_OPS.has(chip.op))
      );
    case "list":
      return chip.kind === "contains";
    case "text":
      return chip.kind === "compare" && (EQUALITY_OPS.has(chip.op) || LIKE_OPS.has(chip.op));
    case "range":
      return chip.kind === "compare" && RANGE_OPS.has(chip.op);
    case "keys":
      return false;
  }
}

/** The Select-tab part of a kind: everything the owning kind writes except the LIKE family. */
function isSelectChip(chip: Chip): boolean {
  return !(chip.kind === "compare" && LIKE_OPS.has(chip.op));
}

/** The conjuncts a facet of `kind` owns on `field` (D4). Text and order as in the filter. */
export function ownedConjuncts(filter: string, field: Field, kind: FacetKind): Conjunct[] {
  return parsedConjuncts(filter)
    .filter((parsed) => owns(parsed.chip, field, kind))
    .map((parsed) => parsed.conjunct);
}

/** The filter without the conjuncts that `drop` selects, joined with `appendFilter` (text kept). */
function keepWhere(filter: string, drop: (parsed: Parsed) => boolean): string {
  return parsedConjuncts(filter)
    .filter((parsed) => !drop(parsed))
    .reduce((joined, parsed) => appendFilter(joined, parsed.conjunct.text), "");
}

function valueText(value: ChipValue): string {
  switch (value.kind) {
    case "string":
      return value.value;
    case "number":
      return value.text;
    case "boolean":
      return value.value ? "true" : "false";
  }
}

/** A selected value as a DSL literal. Booleans are written as boolean literals (D4). */
function literalOf(value: string, kind: SelectKind): ChipValue {
  if (kind === "bool") {
    return { kind: "boolean", value: value === "true" };
  }
  return { kind: "string", value };
}

/** The selection a values facet shows for `field`, and whether the filter edits it as text. */
export function readValues(
  filter: string,
  field: Field,
  kind: SelectKind,
): { selection: Selection; mixed: boolean } {
  const empty: Selection = { mode: "include", values: [], none: false };
  const owned = parsedConjuncts(filter).filter(
    (parsed) => owns(parsed.chip, field, kind) && isSelectChip(parsed.chip),
  );
  if (owned.length === 0) {
    return { selection: empty, mixed: false };
  }
  const [only] = owned;
  if (owned.length > 1 || only === undefined) {
    return { selection: empty, mixed: true };
  }
  const chip = only.chip;
  switch (chip.kind) {
    case "compare":
      return {
        selection: {
          mode: chip.op === "!=" ? "exclude" : "include",
          values: [valueText(chip.value)],
          none: false,
        },
        mixed: false,
      };
    case "in":
      return {
        selection: {
          mode: chip.negated ? "exclude" : "include",
          values: chip.values.map(valueText),
          none: false,
        },
        mixed: false,
      };
    case "null":
      return {
        selection: { mode: chip.negated ? "exclude" : "include", values: [], none: true },
        mixed: false,
      };
    case "contains":
      return { selection: { mode: "include", values: chip.values, none: false }, mixed: false };
    default:
      return { selection: empty, mixed: true };
  }
}

/**
 * The filter with the facet's Select-tab conjuncts replaced by `selection` (D4). Values take
 * precedence over "(none)". An empty selection appends nothing.
 */
export function writeValues(
  filter: string,
  field: Field,
  kind: SelectKind,
  selection: Selection,
): string {
  const base = keepWhere(
    filter,
    (parsed) => owns(parsed.chip, field, kind) && isSelectChip(parsed.chip),
  );
  if (kind === "list") {
    if (selection.values.length === 0) {
      return base;
    }
    return appendFilter(base, serializeChip({ kind: "contains", field, values: selection.values }));
  }
  const exclude = selection.mode === "exclude";
  const values = selection.values.map((value) => literalOf(value, kind));
  if (values.length === 0) {
    if (!selection.none) {
      return base;
    }
    return appendFilter(base, serializeChip({ kind: "null", field, negated: exclude }));
  }
  if (values.length === 1) {
    const [value] = values;
    if (value === undefined) {
      return base;
    }
    return appendFilter(
      base,
      serializeChip({ kind: "compare", field, op: exclude ? "!=" : "=", value }),
    );
  }
  return appendFilter(base, serializeChip({ kind: "in", field, negated: exclude, values }));
}

/** Escapes `\`, `%` and `_` for a LIKE pattern, so the user's text matches literally (D3). */
export function escapeLike(text: string): string {
  return text.replace(/[\\%_]/g, (char) => `\\${char}`);
}

/** The text a Text tab shows for `field`: the first owned LIKE or equality conjunct (D3). */
export function readText(
  filter: string,
  field: Field,
  kind: "values" | "text",
): { op: TextOp; text: string; mixed: boolean } | undefined {
  // A values facet's Text tab owns only the LIKE family; a text facet also owns equality.
  const owned = parsedConjuncts(filter).filter(
    (parsed) =>
      owns(parsed.chip, field, kind) &&
      parsed.chip.kind === "compare" &&
      (kind === "text" || LIKE_OPS.has(parsed.chip.op)),
  );
  const [only] = owned;
  if (only === undefined || only.chip.kind !== "compare") {
    return undefined;
  }
  if (owned.length > 1) {
    return { op: "contains", text: "", mixed: true };
  }
  const { op, value } = only.chip;
  if (value.kind !== "string") {
    return { op: "contains", text: valueText(value), mixed: true };
  }
  if (EQUALITY_OPS.has(op)) {
    return { op: "equals", text: value.value, mixed: op === "!=" };
  }
  const pattern = value.value;
  const negated = op === "NOT LIKE" || op === "NOT ILIKE";
  // Only ILIKE is written by the Text tab; a case-sensitive LIKE is edited as text (D3).
  if (op === "LIKE" || op === "NOT LIKE") {
    return { op: "contains", text: unescapeLike(pattern), mixed: true };
  }
  const inner = pattern.startsWith("%") && pattern.endsWith("%") && pattern.length >= 2;
  if (inner && !hasWildcard(pattern.slice(1, -1))) {
    return {
      op: negated ? "not_contains" : "contains",
      text: unescapeLike(pattern.slice(1, -1)),
      mixed: false,
    };
  }
  if (!negated && pattern.endsWith("%") && !hasWildcard(pattern.slice(0, -1))) {
    return { op: "starts", text: unescapeLike(pattern.slice(0, -1)), mixed: false };
  }
  return { op: "contains", text: unescapeLike(pattern), mixed: true };
}

/** Whether a LIKE body has a `%` or `_` the user did not escape (anything the Text tab would not write). */
function hasWildcard(body: string): boolean {
  for (let index = 0; index < body.length; index += 1) {
    const char = body.charAt(index);
    if (char === "\\") {
      index += 1;
    } else if (char === "%" || char === "_") {
      return true;
    }
  }
  return false;
}

function unescapeLike(pattern: string): string {
  return pattern.replace(/\\([\\%_])/g, "$1");
}

/**
 * The filter with the facet's text conjuncts replaced (D3, D4). Empty text only removes them.
 * `equals` is for the text facet (`trace_id`) only.
 */
export function writeText(
  filter: string,
  field: Field,
  kind: "values" | "text",
  op: TextOp,
  text: string,
): string {
  if (op === "equals" && kind === "values") {
    throw new Error("equals is not an operator of a values facet");
  }
  const base = keepWhere(
    filter,
    (parsed) =>
      owns(parsed.chip, field, kind) &&
      parsed.chip.kind === "compare" &&
      (kind === "text" || LIKE_OPS.has(parsed.chip.op)),
  );
  if (text.length === 0) {
    return base;
  }
  if (op === "equals") {
    return appendFilter(
      base,
      serializeChip({ kind: "compare", field, op: "=", value: { kind: "string", value: text } }),
    );
  }
  const escaped = escapeLike(text);
  const pattern = op === "starts" ? `${escaped}%` : `%${escaped}%`;
  return appendFilter(
    base,
    serializeChip({
      kind: "compare",
      field,
      op: op === "not_contains" ? "NOT ILIKE" : "ILIKE",
      value: { kind: "string", value: pattern },
    }),
  );
}

/** The range a range facet shows: `lo` from `>=`, `hi` from `<` (D5). */
export function readRange(
  filter: string,
  field: Field,
): { lo?: number; hi?: number; mixed: boolean } {
  const owned = parsedConjuncts(filter).filter((parsed) => owns(parsed.chip, field, "range"));
  const lowers = owned.filter(
    (parsed) => parsed.chip.kind === "compare" && LOWER_OPS.has(parsed.chip.op),
  );
  const uppers = owned.filter(
    (parsed) => parsed.chip.kind === "compare" && UPPER_OPS.has(parsed.chip.op),
  );
  const lower = lowers[0]?.chip;
  const upper = uppers[0]?.chip;
  const numberOf = (chip: Chip | undefined): number | undefined =>
    chip?.kind === "compare" && chip.value.kind === "number" ? chip.value.value : undefined;
  // `>` and `<=` cannot be written back by the range inputs, so they show as edited text.
  const exact = owned.every(
    (parsed) =>
      parsed.chip.kind === "compare" &&
      (parsed.chip.op === ">=" || parsed.chip.op === "<") &&
      parsed.chip.value.kind === "number",
  );
  const mixed = !exact || lowers.length > 1 || uppers.length > 1;
  return {
    ...(numberOf(lower) === undefined ? {} : { lo: numberOf(lower) }),
    ...(numberOf(upper) === undefined ? {} : { hi: numberOf(upper) }),
    mixed,
  };
}

/** The filter with the range conjuncts of `field` replaced by `>= lo` and `< hi` (D5). */
export function writeRange(filter: string, field: Field, lo?: number, hi?: number): string {
  let next = keepWhere(filter, (parsed) => owns(parsed.chip, field, "range"));
  if (lo !== undefined) {
    next = appendFilter(next, rangeConjunct(field, ">=", lo));
  }
  if (hi !== undefined) {
    next = appendFilter(next, rangeConjunct(field, "<", hi));
  }
  return next;
}

function rangeConjunct(field: Field, op: CompareOp, value: number): string {
  const text = formatBound(value);
  return serializeChip({ kind: "compare", field, op, value: { kind: "number", value, text } });
}

/** A number as a DSL literal: at most 6 significant digits, no exponent, no trailing zeros. */
export function formatBound(value: number): string {
  // The range UI rejects non-finite bounds (D5); this is a safety net that never throws.
  if (!Number.isFinite(value)) {
    return "0";
  }
  const magnitude = Math.abs(value);
  // Below 1e-9 the bound would print as 0 or in exponent form: it is written as 0 (D5).
  if (magnitude < 1e-9) {
    return "0";
  }
  const rounded = Number(value.toPrecision(6));
  if (magnitude >= 1e21) {
    return BigInt(Math.round(rounded)).toString();
  }
  const digits = Math.floor(Math.log10(magnitude)) + 1;
  const decimals = Math.max(0, 6 - digits);
  const fixed = rounded.toFixed(decimals);
  return fixed.includes(".") ? fixed.replace(/\.?0+$/, "") : fixed;
}

/**
 * The filter without every conjunct the field's facets own, of any kind (D4). Facet counts are
 * fetched with it, and a section's Clear removes them.
 */
export function withoutField(filter: string, field: Field): string {
  return keepWhere(filter, ({ chip }) => {
    const own = chipField(chip);
    return own !== undefined && sameField(own, field);
  });
}

/** Clear of a section: the same conjuncts as `withoutField` (every kind). */
export const clearField = withoutField;

export type PresetId = "errors" | "slow" | "expensive" | "llm";

export type PresetDef = {
  id: PresetId;
  label: string;
  /** The conjunct a preset toggles, when it has a fixed one. */
  conjunct?: string;
  /** The field whose p95 sets the bound of a Slow or Expensive preset (D6). */
  p95Field?: string;
};

/** The presets of each list (D6). Has score is a menu of the score definitions, not listed here. */
export const PRESETS: Record<FacetView, PresetDef[]> = {
  traces: [
    { id: "errors", label: "Errors", conjunct: "status = 'error'" },
    { id: "slow", label: "Slow", p95Field: "duration_ms" },
    { id: "expensive", label: "Expensive", p95Field: "cost_total" },
  ],
  spans: [
    { id: "errors", label: "Errors", conjunct: "status = 'error'" },
    { id: "slow", label: "Slow", p95Field: "duration_ms" },
    { id: "expensive", label: "Expensive", p95Field: "cost_total" },
    { id: "llm", label: "LLM", conjunct: "span_kind = 'llm'" },
  ],
  sessions: [
    { id: "errors", label: "Errors", conjunct: "error_count > 0" },
    { id: "slow", label: "Slow", p95Field: "duration_ms" },
    { id: "expensive", label: "Expensive", p95Field: "cost_total" },
  ],
};

/** A conjunct as the DSL prints it, so the same condition compares equal whatever its spelling. */
function normalized(text: string): string {
  return serializeChip(parseChip(text));
}

/** Whether the filter holds the preset: its conjunct, or an owned lower bound on its field. */
export function presetActive(filter: string, preset: PresetDef): boolean {
  if (preset.conjunct !== undefined) {
    const wanted = normalized(preset.conjunct);
    return parsedConjuncts(filter).some((parsed) => normalized(parsed.conjunct.text) === wanted);
  }
  return parsedConjuncts(filter).some((parsed) => isLowerBoundOn(parsed.chip, preset.p95Field));
}

function isLowerBoundOn(chip: Chip, column: string | undefined): boolean {
  return (
    chip.kind === "compare" &&
    LOWER_OPS.has(chip.op) &&
    chip.field.column === column &&
    !("key" in chip.field)
  );
}

/** A p95 rounded to 3 significant digits, as the Slow and Expensive presets write it (D6). */
export function roundP95(value: number): number {
  return Number(value.toPrecision(3));
}

/**
 * The filter with the preset toggled (D6). Turning a Slow or Expensive preset on needs its p95;
 * without one the filter is returned unchanged.
 */
export function togglePreset(filter: string, preset: PresetDef, p95?: number): string {
  const active = presetActive(filter, preset);
  if (preset.conjunct !== undefined) {
    if (active) {
      const wanted = normalized(preset.conjunct);
      return keepWhere(filter, (parsed) => normalized(parsed.conjunct.text) === wanted);
    }
    return appendFilter(filter, preset.conjunct);
  }
  if (active) {
    return keepWhere(filter, (parsed) => isLowerBoundOn(parsed.chip, preset.p95Field));
  }
  if (p95 === undefined || preset.p95Field === undefined) {
    return filter;
  }
  return appendFilter(filter, rangeConjunct({ column: preset.p95Field }, ">=", roundP95(p95)));
}

/** The list kinds a filter can be carried between (D8). */
export type ListKind = "traces" | "spans" | "sessions" | "users";

/** The filter view of a list: `users` counts traces. */
export function filterViewOf(kind: ListKind): FacetView {
  return kind === "users" ? "traces" : kind;
}

/** The identifiers a carried conjunct may name, per filter view (D8). */
const CARRY: Record<FacetView, ReadonlySet<string>> = {
  traces: new Set([
    "session_id",
    "user_id",
    "environment",
    "release",
    "tags",
    "metadata",
    "trace_id",
  ]),
  spans: new Set([
    "session_id",
    "user_id",
    "environment",
    "release",
    "tags",
    "metadata",
    "trace_id",
  ]),
  sessions: new Set(["user_id", "tags"]),
};

/**
 * Whether a conjunct of a list of `from` is kept on a list of `to` (D8). Within one filter view
 * every conjunct is kept. Across views, every identifier it names must be carried by both, and a
 * function call (a score) drops it.
 */
export function carryConjunct(text: string, from: ListKind, to: ListKind): boolean {
  const fromView = filterViewOf(from);
  const toView = filterViewOf(to);
  if (fromView === toView) {
    return true;
  }
  const tokens = tokenize(text);
  for (const [index, token] of tokens.entries()) {
    if (token.kind !== "ident") {
      continue;
    }
    if (tokens[index + 1]?.kind === "lparen") {
      return false;
    }
    const name = token.text.toLowerCase();
    if (!CARRY[fromView].has(name) || !CARRY[toView].has(name)) {
      return false;
    }
  }
  return true;
}

/** The conjuncts of `filter` a list of `to` keeps from a list of `from`, and how many it dropped (D8). */
export function carryFilter(
  filter: string,
  from: ListKind,
  to: ListKind,
): { kept: string; dropped: number } {
  let kept = "";
  let dropped = 0;
  for (const conjunct of splitConjuncts(filter)) {
    if (carryConjunct(conjunct.text, from, to)) {
      kept = appendFilter(kept, conjunct.text);
    } else {
      dropped += 1;
    }
  }
  return { kept, dropped };
}

/** The conjuncts on any key of a map column (`metadata['k']`), of any kind (keys sections, D3). */
function keyedOn(chip: Chip, column: string): Field | undefined {
  const own = chipField(chip);
  return own !== undefined && "key" in own && own.column === column ? own : undefined;
}

/** The conjuncts that name a key of `column`, in filter order. */
export function ownedKeyConjuncts(filter: string, column: string): Conjunct[] {
  return parsedConjuncts(filter)
    .filter((parsed) => keyedOn(parsed.chip, column) !== undefined)
    .map((parsed) => parsed.conjunct);
}

/** The distinct keys of `column` that the filter names, in filter order. */
export function keysInFilter(filter: string, column: string): string[] {
  const keys: string[] = [];
  for (const parsed of parsedConjuncts(filter)) {
    const own = keyedOn(parsed.chip, column);
    if (own !== undefined && "key" in own && !keys.includes(own.key)) {
      keys.push(own.key);
    }
  }
  return keys;
}

/** Clear of a keys section: removes every conjunct on a key of `column` (any key). */
export function clearKeys(filter: string, column: string): string {
  return keepWhere(filter, ({ chip }) => keyedOn(chip, column) !== undefined);
}
