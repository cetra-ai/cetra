import type { Schema } from "@/api/types";

/** A dashboard with its definition (`GET …/dashboards/{id}`). */
export type Dashboard = Schema<"Dashboard">;
/** A dashboard without its definition, as the list returns it. */
export type DashboardSummary = Schema<"DashboardSummary">;
/** The stored definition: the widgets, in order. */
export type Definition = Schema<"Definition">;
/** One widget of a dashboard. */
export type Widget = Schema<"Widget">;
/** Position and size of a widget on the 12-column grid. */
export type Layout = Schema<"Layout">;
/** Chart settings of a chart widget. */
export type Chart = Schema<"Chart">;
/** Type of a chart widget. */
export type ChartType = Schema<"ChartType">;
/** Kind of a widget. */
export type WidgetKind = Schema<"WidgetKind">;
/** A widget's query. */
export type WidgetQuery = Schema<"WidgetQueryBody">;
/** One measure of a widget query. */
export type Measure = Schema<"MeasureBody">;
/** How a measure aggregates its rows. */
export type Agg = Schema<"AggBody">;
/** The split of a widget query by one field. */
export type Breakdown = Schema<"BreakdownBody">;
/** The view a widget query reads. */
export type WidgetSource = Schema<"WidgetSourceBody">;
/** Body of `POST …/dashboards`. */
export type CreateDashboard = Schema<"CreateDashboard">;
/** Body of `PATCH …/dashboards/{id}`. */
export type UpdateDashboard = Schema<"UpdateDashboard">;
/** Body of `PUT …/dashboards/overview`. */
export type SaveOverview = Schema<"SaveOverview">;
