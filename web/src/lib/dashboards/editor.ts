import { uuidv7 } from "@/lib/uuid";
import { breakdownFields, fieldsForAgg, isMapKeyField, mapColumns } from "./fields";
import type {
  Agg,
  Breakdown,
  Chart,
  ChartType,
  Measure,
  Widget,
  WidgetQuery,
  WidgetSource,
} from "./types";

/** The widget editor's state: every control as text or a flag, converted by `widgetOf`. */
export type MeasureForm = {
  /** A key for the editor's row, stable while the row stays. Not part of the widget. */
  rowId: string;
  agg: Agg;
  /** A numeric field or, for `count_distinct`, a breakdown field. Empty for `count` and `rate`. */
  field: string;
  /** `column['key']` for `count_distinct`, typed by hand. Wins over `field` when set. */
  mapKey: string;
  /** The condition, a filter over the source. Required for `rate`. */
  where: string;
  label: string;
};

export type WidgetForm = {
  title: string;
  source: WidgetSource;
  measures: MeasureForm[];
  breakdown: boolean;
  breakdownField: string;
  breakdownMapKey: string;
  /** Text, so a half-typed number is shown as typed. */
  limit: string;
  other: boolean;
  filter: string;
  type: ChartType;
  stacked: boolean;
  split: boolean;
  /** Text: empty means no fading. */
  fadeBelow: string;
  share: boolean;
};

export const SOURCES: readonly WidgetSource[] = ["traces", "spans", "scores"];
export const CHART_TYPES: readonly ChartType[] = [
  "line",
  "bar",
  "area",
  "bar_h",
  "number",
  "table",
];
export const MAX_MEASURES = 6;

export const SOURCE_LABELS: Record<WidgetSource, string> = {
  traces: "Traces",
  spans: "Spans",
  scores: "Scores",
};

export const AGG_LABELS: Record<Agg, string> = {
  count: "Count",
  count_distinct: "Distinct count",
  sum: "Sum",
  avg: "Average",
  min: "Minimum",
  max: "Maximum",
  p50: "p50",
  p75: "p75",
  p90: "p90",
  p95: "p95",
  p99: "p99",
  rate: "Rate",
};

export const CHART_LABELS: Record<ChartType, string> = {
  line: "Line",
  bar: "Bar",
  area: "Area",
  bar_h: "Horizontal bar",
  number: "Number",
  table: "Table",
};

/** The aggregates in the order the editor lists them. */
export const AGGS: readonly Agg[] = [
  "count",
  "count_distinct",
  "sum",
  "avg",
  "min",
  "max",
  "p50",
  "p75",
  "p90",
  "p95",
  "p99",
  "rate",
];

/** Which options a chart type has (spec 04 § Dashboards). */
export function optionsOf(type: ChartType): {
  stacked: boolean;
  split: boolean;
  fadeBelow: boolean;
  share: boolean;
} {
  return {
    stacked: type === "bar" || type === "area",
    split: type === "line",
    fadeBelow: type === "bar",
    share: type === "table",
  };
}

/** The field an aggregate starts with: the first one it offers, or empty. */
export function defaultField(source: WidgetSource, agg: Agg): string {
  return fieldsForAgg(source, agg)[0] ?? "";
}

/** A new chart's form: traces, a count, a line. */
export function blankForm(): WidgetForm {
  return {
    title: "",
    source: "traces",
    measures: [{ rowId: uuidv7(), agg: "count", field: "", mapKey: "", where: "", label: "" }],
    breakdown: false,
    breakdownField: "",
    breakdownMapKey: "",
    limit: "5",
    other: true,
    filter: "",
    type: "line",
    stacked: false,
    split: false,
    fadeBelow: "",
    share: false,
  };
}

/** The form of a chart widget. A widget without a chart (a heading) gets the blank form. */
export function formOf(widget: Widget): WidgetForm {
  const { chart, query } = widget;
  if (chart === undefined || query === undefined) {
    return { ...blankForm(), title: widget.title };
  }
  const source = query.source;
  const breakdown = query.breakdown;
  const breakdownIsKey = breakdown !== undefined && isMapKeyField(source, breakdown.field);
  return {
    title: widget.title,
    source,
    measures: query.measures.map((measure) => {
      const key = measure.field !== undefined && isMapKeyField(source, measure.field);
      return {
        rowId: uuidv7(),
        agg: measure.agg,
        field: key ? "" : (measure.field ?? defaultField(source, measure.agg)),
        mapKey: key ? (measure.field ?? "") : "",
        where: measure.where ?? "",
        label: measure.label ?? "",
      };
    }),
    breakdown: breakdown !== undefined,
    breakdownField: breakdown && !breakdownIsKey ? breakdown.field : "",
    breakdownMapKey: breakdownIsKey ? breakdown.field : "",
    limit: String(breakdown?.limit ?? 5),
    other: breakdown?.other !== false,
    filter: query.filter ?? "",
    type: chart.type,
    stacked: chart.stacked ?? false,
    split: chart.split ?? false,
    fadeBelow: chart.fade_below === undefined ? "" : String(chart.fade_below),
    share: chart.share ?? false,
  };
}

/**
 * The widget a form describes, with the id and layout of `base`. Only the options of the chart
 * type are written. A number that does not parse becomes 0, which the rules refuse, so a typo
 * is never sent as `null`.
 */
export function widgetOf(form: WidgetForm, base: Widget): Widget {
  const options = optionsOf(form.type);
  const chart: Chart = {
    type: form.type,
    ...(options.stacked && form.stacked ? { stacked: true } : {}),
    ...(options.split && form.split ? { split: true } : {}),
    ...(options.fadeBelow && form.fadeBelow.trim() !== ""
      ? { fade_below: numberOf(form.fadeBelow) }
      : {}),
    ...(options.share && form.share ? { share: true } : {}),
  };
  const query: WidgetQuery = {
    source: form.source,
    measures: form.measures.map((measure) => measureOf(measure)),
    ...(form.filter.trim() !== "" ? { filter: form.filter.trim() } : {}),
    ...(form.breakdown ? { breakdown: breakdownOf(form) } : {}),
  };
  return {
    id: base.id,
    title: form.title.trim(),
    kind: "chart",
    chart,
    query,
    layout: base.layout,
  };
}

function measureOf(form: MeasureForm): Measure {
  const field = fieldOf(form);
  const where = form.where.trim();
  const label = form.label.trim();
  return {
    agg: form.agg,
    ...(field !== undefined ? { field } : {}),
    ...(where !== "" ? { where } : {}),
    ...(label !== "" ? { label } : {}),
  };
}

/** The field a measure aggregates: none for `count` and `rate`, the map key when one is typed. */
function fieldOf(form: MeasureForm): string | undefined {
  if (form.agg === "count" || form.agg === "rate") {
    return undefined;
  }
  const key = form.agg === "count_distinct" ? form.mapKey.trim() : "";
  const field = key !== "" ? key : form.field;
  return field === "" ? undefined : field;
}

function breakdownOf(form: WidgetForm): Breakdown {
  const key = form.breakdownMapKey.trim();
  const field = key !== "" ? key : form.breakdownField;
  return {
    field,
    limit: form.limit.trim() === "" ? 5 : numberOf(form.limit),
    ...(form.other ? {} : { other: false }),
  };
}

function numberOf(text: string): number {
  const value = Number(text);
  return Number.isNaN(value) ? 0 : value;
}

/**
 * Keeps each control valid after the source changes. The filter and each measure's condition go,
 * since they are written over the old source. A measure keeps its field when the new source has
 * it for the aggregate, else takes the first one; a map key goes when its column is not one of the
 * new source's; the breakdown field goes when the source has no such field.
 */
export function withSource(form: WidgetForm, source: WidgetSource): WidgetForm {
  const fields = breakdownFields(source);
  return {
    ...form,
    source,
    filter: "",
    measures: form.measures.map((measure) => {
      const allowed = fieldsForAgg(source, measure.agg);
      const fieldKept = allowed.includes(measure.field);
      return {
        ...measure,
        where: "",
        field: fieldKept ? measure.field : (allowed[0] ?? ""),
        mapKey: fieldKept && mapKeyOn(source, measure.mapKey) ? measure.mapKey : "",
      };
    }),
    breakdownField: fields.includes(form.breakdownField) ? form.breakdownField : "",
    breakdownMapKey: mapKeyOn(source, form.breakdownMapKey) ? form.breakdownMapKey : "",
  };
}

/** Whether a typed map key (`column['key']`) names a map column of the source. */
function mapKeyOn(source: WidgetSource, text: string): boolean {
  const column = text.trim().split("[")[0] ?? "";
  return mapColumns(source).includes(column);
}

/** Whether a source has map columns, so `column['key']` is offered. */
export function hasMapColumns(source: WidgetSource): boolean {
  return mapColumns(source).length > 0;
}
