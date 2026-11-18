import { breakdownFields, countDistinctFields, isMapKeyField, numericFields } from "./fields";
import type { Chart, Definition, Measure, Widget, WidgetQuery, WidgetSource } from "./types";

/** One invalid field, with the path the server would report (`definition.widgets[0].id`). */
export type FieldIssue = { field: string; message: string };

const MAX_NAME_CHARS = 100;
const MAX_DESCRIPTION_CHARS = 500;
const MAX_WIDGETS = 50;
const MAX_DEFINITION_BYTES = 262144;
const MAX_ID_CHARS = 64;
const MAX_TITLE_CHARS = 100;
const MAX_LABEL_CHARS = 60;
const MAX_MEASURES = 6;
const MAX_BREAKDOWN_LIMIT = 20;
const DEFAULT_BREAKDOWN_LIMIT = 5;
const MAX_SPLIT_GROUPS = 6;

/** Characters, as Rust's `chars().count()` counts them. */
function chars(text: string): number {
  return Array.from(text).length;
}

/**
 * The rules of spec 04 § Dashboards, as `routes/dashboards.rs` checks them. The paths are the
 * server's. The filter expressions are not checked here: the server parses them.
 *
 * `definition` is optional: a PATCH that omits it checks only the other fields.
 */
export function validateDashboard(input: {
  name: string;
  description?: string;
  definition?: Definition;
}): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const name = input.name.trim();
  if (chars(name) < 1 || chars(name) > MAX_NAME_CHARS) {
    issues.push({ field: "name", message: "must be 1 to 100 characters" });
  }
  if (input.description !== undefined && chars(input.description.trim()) > MAX_DESCRIPTION_CHARS) {
    issues.push({ field: "description", message: "must be at most 500 characters" });
  }
  if (input.definition !== undefined) {
    issues.push(...validateDefinition(input.definition));
  }
  return issues;
}

/** The widgets of a definition, with the paths under `definition`. */
export function validateDefinition(definition: Definition): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const bytes = new TextEncoder().encode(JSON.stringify(definition)).length;
  if (bytes > MAX_DEFINITION_BYTES) {
    issues.push({ field: "definition", message: "must be at most 262144 bytes" });
  }
  if (definition.widgets.length > MAX_WIDGETS) {
    issues.push({ field: "definition.widgets", message: "must have at most 50 widgets" });
  }
  const ids = new Set<string>();
  definition.widgets.forEach((widget, index) => {
    issues.push(...validateWidget(widget, `definition.widgets[${index}]`, ids));
  });
  return issues;
}

function validateWidget(widget: Widget, base: string, ids: Set<string>): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const idOk = /^[A-Za-z0-9_-]+$/.test(widget.id) && chars(widget.id) <= MAX_ID_CHARS;
  if (!idOk) {
    issues.push({
      field: `${base}.id`,
      message: "must be 1 to 64 characters of letters, digits, _ or -",
    });
  } else if (ids.has(widget.id)) {
    issues.push({ field: `${base}.id`, message: "must be unique in the dashboard" });
  } else {
    ids.add(widget.id);
  }

  if (widget.kind === "heading") {
    const length = chars(widget.title.trim());
    if (length < 1 || length > MAX_TITLE_CHARS) {
      issues.push({
        field: `${base}.title`,
        message: "a heading needs 1 to 100 characters",
      });
    }
  } else if (chars(widget.title) > MAX_TITLE_CHARS) {
    issues.push({ field: `${base}.title`, message: "must be at most 100 characters" });
  }

  issues.push(...validateLayout(widget, base));

  if (widget.kind === "heading") {
    if (widget.chart !== undefined) {
      issues.push({ field: `${base}.chart`, message: "a heading has no chart" });
    }
    if (widget.query !== undefined) {
      issues.push({ field: `${base}.query`, message: "a heading has no query" });
    }
    return issues;
  }
  if (widget.chart === undefined) {
    issues.push({ field: `${base}.chart`, message: "a chart needs chart" });
  } else {
    issues.push(...validateChartOptions(widget.chart, base));
  }
  if (widget.query === undefined) {
    issues.push({ field: `${base}.query`, message: "a chart needs a query" });
  } else {
    issues.push(...validateQuery(widget.query, widget, base));
  }
  return issues;
}

function validateLayout(widget: Widget, base: string): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const { x, y, w, h } = widget.layout;
  if (!(x >= 0 && x <= 11)) {
    issues.push({ field: `${base}.layout.x`, message: "must be 0 to 11" });
  }
  if (!(y >= 0 && y <= 1000)) {
    issues.push({ field: `${base}.layout.y`, message: "must be 0 to 1000" });
  }
  if (!(w >= 1 && w <= 12)) {
    issues.push({ field: `${base}.layout.w`, message: "must be 1 to 12" });
  } else if (x + w > 12) {
    issues.push({ field: `${base}.layout.w`, message: "x + w must be at most 12" });
  }
  if (!(h >= 1 && h <= 40)) {
    issues.push({ field: `${base}.layout.h`, message: "must be 1 to 40" });
  }
  return issues;
}

/** The per-type rules of a chart's settings. */
function validateChartOptions(chart: Chart, base: string): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const type = chart.type;
  if (chart.stacked !== undefined && type !== "bar" && type !== "area") {
    issues.push({ field: `${base}.chart.stacked`, message: "only bar and area charts stack" });
  }
  if (chart.split !== undefined && type !== "line") {
    issues.push({ field: `${base}.chart.split`, message: "only line charts split" });
  }
  if (chart.fade_below !== undefined) {
    if (type !== "bar") {
      issues.push({ field: `${base}.chart.fade_below`, message: "only bar charts fade" });
    } else if (!(chart.fade_below >= 1 && chart.fade_below <= 1000)) {
      issues.push({ field: `${base}.chart.fade_below`, message: "must be 1 to 1000" });
    }
  }
  if (chart.share !== undefined && type !== "table") {
    issues.push({ field: `${base}.chart.share`, message: "only table charts show a share" });
  }
  return issues;
}

/** The query's shape, its labels, and the chart-type rules that read the query. */
function validateQuery(query: WidgetQuery, widget: Widget, base: string): FieldIssue[] {
  const queryBase = `${base}.query`;
  const issues = validateShape(query, queryBase);
  query.measures.forEach((measure, index) => {
    if (measure.label !== undefined && chars(measure.label) > MAX_LABEL_CHARS) {
      issues.push({
        field: `${queryBase}.measures[${index}].label`,
        message: "must be at most 60 characters",
      });
    }
  });
  const chart = widget.chart;
  if (chart === undefined) {
    return issues;
  }
  if (chart.type === "line" && chart.split === true) {
    if (query.breakdown === undefined) {
      issues.push({ field: `${queryBase}.breakdown`, message: "a split chart needs a breakdown" });
    } else if ((query.breakdown.limit ?? DEFAULT_BREAKDOWN_LIMIT) > MAX_SPLIT_GROUPS) {
      issues.push({
        field: `${queryBase}.breakdown.limit`,
        message: "a split chart has at most 6 groups",
      });
    }
  }
  if (chart.type === "bar_h" && query.breakdown === undefined) {
    issues.push({
      field: `${queryBase}.breakdown`,
      message: "a horizontal bar chart needs a breakdown",
    });
  }
  if (chart.type === "number") {
    if (query.measures.length !== 1) {
      issues.push({
        field: `${queryBase}.measures`,
        message: "a number chart has exactly one measure",
      });
    }
    if (query.breakdown !== undefined) {
      issues.push({ field: `${queryBase}.breakdown`, message: "a number chart has no breakdown" });
    }
  }
  return issues;
}

/**
 * The fields of a query, as `validate_shape` checks them: 1 to 6 measures, each measure's field
 * and condition for its aggregate, and the breakdown's field and limit.
 */
function validateShape(query: WidgetQuery, queryBase: string): FieldIssue[] {
  const source = query.source;
  const issues: FieldIssue[] = [];
  if (query.measures.length < 1 || query.measures.length > MAX_MEASURES) {
    issues.push({ field: `${queryBase}.measures`, message: "must have 1 to 6 measures" });
  }
  query.measures.forEach((measure, index) => {
    issues.push(...validateMeasure(measure, source, `${queryBase}.measures[${index}]`));
  });
  if (query.breakdown !== undefined) {
    const { field, limit } = query.breakdown;
    if (!breakdownFields(source).includes(field) && !isMapKeyField(source, field)) {
      issues.push({
        field: `${queryBase}.breakdown.field`,
        message: "not a breakdown field of this source",
      });
    }
    if (limit !== undefined && (limit < 1 || limit > MAX_BREAKDOWN_LIMIT)) {
      issues.push({ field: `${queryBase}.breakdown.limit`, message: "must be 1 to 20" });
    }
  }
  return issues;
}

function validateMeasure(measure: Measure, source: WidgetSource, base: string): FieldIssue[] {
  const issues: FieldIssue[] = [];
  const fieldPath = `${base}.field`;
  const wherePath = `${base}.where`;
  const hasCondition = measure.where !== undefined && measure.where.trim() !== "";
  const field = measure.field;
  switch (measure.agg) {
    case "count":
      if (field !== undefined) {
        issues.push({ field: fieldPath, message: "count takes no field" });
      }
      break;
    case "rate":
      if (field !== undefined) {
        issues.push({ field: fieldPath, message: "rate takes no field" });
      }
      if (!hasCondition) {
        issues.push({ field: wherePath, message: "rate needs a where" });
      }
      break;
    case "count_distinct":
      if (field === undefined) {
        issues.push({ field: fieldPath, message: "count_distinct needs a field" });
      } else if (!countDistinctFields(source).includes(field) && !isMapKeyField(source, field)) {
        issues.push({ field: fieldPath, message: "count_distinct does not take this field" });
      }
      break;
    default:
      if (field === undefined) {
        issues.push({ field: fieldPath, message: "needs a numeric field" });
      } else if (!numericFields(source).includes(field)) {
        issues.push({ field: fieldPath, message: "not a numeric field of this source" });
      }
  }
  return issues;
}
