import { type ReactNode, useEffect, useMemo, useState } from "react";
import { FilterBar } from "@/components/filter-bar/filter-bar";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { issueWidgetIndex, issueWithin } from "@/lib/dashboards/draft";
import {
  AGG_LABELS,
  AGGS,
  CHART_LABELS,
  CHART_TYPES,
  defaultField,
  formOf,
  hasMapColumns,
  MAX_MEASURES,
  optionsOf,
  SOURCE_LABELS,
  SOURCES,
  type WidgetForm,
  widgetOf,
  withSource,
} from "@/lib/dashboards/editor";
import { breakdownFields, fieldsForAgg, mapColumns } from "@/lib/dashboards/fields";
import type { Definition, Widget as WidgetModel } from "@/lib/dashboards/types";
import { validateDefinition } from "@/lib/dashboards/validate";
import { Widget, type WidgetContext } from "./widget";

/** How long the preview waits after the last change before it queries. */
const PREVIEW_DELAY_MS = 400;
const MAX_TITLE_CHARS = 100;

/**
 * The widget editor. `base` is the widget being edited, or a new one with its id and layout;
 * `draft` is the dashboard it is part of, and `index` the widget's place in it. The client rules
 * of the server check the widget in its place, so the messages are the server's.
 */
export function WidgetEditor({
  open,
  onOpenChange,
  base,
  draft,
  index,
  context,
  onApply,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  base: WidgetModel;
  draft: Definition;
  index: number;
  context: WidgetContext;
  onApply: (widget: WidgetModel) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-3xl">
        <EditorForm
          base={base}
          draft={draft}
          index={index}
          context={context}
          onCancel={() => onOpenChange(false)}
          onApply={onApply}
        />
      </DialogContent>
    </Dialog>
  );
}

function EditorForm({
  base,
  draft,
  index,
  context,
  onCancel,
  onApply,
}: {
  base: WidgetModel;
  draft: Definition;
  index: number;
  context: WidgetContext;
  onCancel: () => void;
  onApply: (widget: WidgetModel) => void;
}) {
  const [form, setForm] = useState<WidgetForm>(() => formOf(base));
  const candidate = useMemo(() => widgetOf(form, base), [form, base]);
  const issues = useMemo(() => issuesOf(draft, index, candidate), [draft, index, candidate]);
  const preview = useDebounced(candidate, PREVIEW_DELAY_MS);
  // The preview is the candidate once it has settled and only while the candidate is valid.
  const previewReady = issues.length === 0 && preview === candidate;
  const update = (patch: Partial<WidgetForm>) => setForm((current) => ({ ...current, ...patch }));
  const source = form.source;

  return (
    <form
      className="contents"
      onSubmit={(event) => {
        // A submit from a form inside this one (the filter bar's chip popover is portalled, so it
        // reaches here through React) is that form's, not this editor's.
        if (event.target !== event.currentTarget) {
          return;
        }
        event.preventDefault();
        if (issues.length === 0) {
          onApply(candidate);
        }
      }}
    >
      <DialogHeader>
        <DialogTitle>{base.chart ? "Edit widget" : "New widget"}</DialogTitle>
        <DialogDescription>
          Pick the data, the chart and what it shows. The preview uses the dashboard's range and
          filter.
        </DialogDescription>
      </DialogHeader>

      <div className="flex flex-col gap-1.5">
        <Label htmlFor="widget-title">Title</Label>
        <Input
          id="widget-title"
          value={form.title}
          maxLength={MAX_TITLE_CHARS}
          onChange={(event) => update({ title: event.target.value })}
        />
      </div>

      <div className="flex flex-col gap-1.5">
        <Label htmlFor="widget-source">Source</Label>
        <select
          id="widget-source"
          className={selectClass}
          value={source}
          onChange={(event) => {
            const next = SOURCES.find((item) => item === event.target.value);
            if (next) {
              setForm((current) => withSource(current, next));
            }
          }}
        >
          {SOURCES.map((item) => (
            <option key={item} value={item}>
              {SOURCE_LABELS[item]}
            </option>
          ))}
        </select>
      </div>

      <fieldset className="flex flex-col gap-3">
        <legend className="text-body font-medium">Measures</legend>
        {form.measures.map((measure, position) => (
          <MeasureRow
            key={measure.rowId}
            position={position}
            source={source}
            measure={measure}
            canRemove={form.measures.length > 1}
            onChange={(next) =>
              setForm((current) => ({
                ...current,
                measures: current.measures.map((item, at) => (at === position ? next : item)),
              }))
            }
            onRemove={() =>
              setForm((current) => ({
                ...current,
                measures: current.measures.filter((_, at) => at !== position),
              }))
            }
          />
        ))}
        {form.measures.length < MAX_MEASURES ? (
          <div>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() =>
                setForm((current) => ({
                  ...current,
                  measures: [
                    ...current.measures,
                    {
                      rowId: crypto.randomUUID(),
                      agg: "count",
                      field: "",
                      mapKey: "",
                      where: "",
                      label: "",
                    },
                  ],
                }))
              }
            >
              Add measure
            </Button>
          </div>
        ) : null}
      </fieldset>

      <fieldset className="flex flex-col gap-3">
        <legend className="text-body font-medium">Breakdown</legend>
        <CheckField
          id="widget-breakdown-on"
          checked={form.breakdown}
          onChange={(value) => update({ breakdown: value })}
        >
          Break down by a field
        </CheckField>
        {form.breakdown ? (
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="widget-breakdown-field">Breakdown field</Label>
              <select
                id="widget-breakdown-field"
                className={selectClass}
                value={form.breakdownField}
                onChange={(event) => update({ breakdownField: event.target.value })}
              >
                <option value="">Choose a field</option>
                {breakdownFields(source).map((field) => (
                  <option key={field} value={field}>
                    {field}
                  </option>
                ))}
              </select>
            </div>
            {hasMapColumns(source) ? (
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="widget-breakdown-key">Map key</Label>
                <Input
                  id="widget-breakdown-key"
                  placeholder={`${mapColumns(source)[0]}['key']`}
                  value={form.breakdownMapKey}
                  onChange={(event) => update({ breakdownMapKey: event.target.value })}
                />
              </div>
            ) : null}
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="widget-breakdown-limit">Groups (1 to 20)</Label>
              <Input
                id="widget-breakdown-limit"
                type="number"
                min={1}
                max={20}
                value={form.limit}
                onChange={(event) => update({ limit: event.target.value })}
              />
            </div>
            <CheckField
              id="widget-breakdown-other"
              checked={form.other}
              onChange={(value) => update({ other: value })}
            >
              Group the rest as Other
            </CheckField>
          </div>
        ) : null}
      </fieldset>

      <div className="flex flex-col gap-1.5">
        <span className="text-body font-medium">Widget filter</span>
        <FilterBar
          view={source}
          value={form.filter}
          onApply={(text) => update({ filter: text })}
          scope={{
            from: context.from,
            to: context.to,
            includeEval: context.includeEval,
            includeExperiments: context.includeExperiments,
          }}
        />
      </div>

      <fieldset className="flex flex-col gap-3">
        <legend className="text-body font-medium">Chart</legend>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="widget-chart-type">Chart type</Label>
          <select
            id="widget-chart-type"
            className={selectClass}
            value={form.type}
            onChange={(event) => {
              const next = CHART_TYPES.find((item) => item === event.target.value);
              if (next) {
                update({ type: next });
              }
            }}
          >
            {CHART_TYPES.map((type) => (
              <option key={type} value={type}>
                {CHART_LABELS[type]}
              </option>
            ))}
          </select>
        </div>
        <ChartOptions form={form} update={update} />
      </fieldset>

      <section className="flex flex-col gap-2" aria-label="Preview">
        <span className="text-body font-medium">Preview</span>
        {previewReady ? (
          <Widget widget={preview} context={context} />
        ) : (
          <p className="text-caption text-muted-foreground">
            {issues.length === 0
              ? "Updating the preview…"
              : "Fix the highlighted fields to see the preview."}
          </p>
        )}
      </section>

      {issues.length > 0 ? (
        <ul className="flex flex-col gap-1 text-caption text-destructive">
          {issues.map((issue) => (
            <li key={`${issue.field}:${issue.message}`}>
              {issueWithin(issue.field)}: {issue.message}
            </li>
          ))}
        </ul>
      ) : null}

      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" disabled={issues.length > 0}>
          Apply
        </Button>
      </DialogFooter>
    </form>
  );
}

/** A checkbox with its label, as one control. */
function CheckField({
  id,
  checked,
  onChange,
  children,
}: {
  id: string;
  checked: boolean;
  onChange: (value: boolean) => void;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center gap-2 text-body">
      <Checkbox id={id} checked={checked} onCheckedChange={(next) => onChange(next === true)} />
      <Label htmlFor={id} className="font-normal">
        {children}
      </Label>
    </div>
  );
}

const selectClass =
  "h-9 w-full min-w-0 rounded-md border border-input bg-transparent px-2.5 text-ui shadow-xs outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30";

/** One measure: its aggregate, field or map key, condition and label. */
function MeasureRow({
  position,
  source,
  measure,
  canRemove,
  onChange,
  onRemove,
}: {
  position: number;
  source: WidgetForm["source"];
  measure: WidgetForm["measures"][number];
  canRemove: boolean;
  onChange: (next: WidgetForm["measures"][number]) => void;
  onRemove: () => void;
}) {
  const id = `measure-${position}`;
  const fields = fieldsForAgg(source, measure.agg);
  const keyed = measure.agg === "count_distinct" && hasMapColumns(source);
  return (
    <div className="grid gap-3 rounded-md border p-3 sm:grid-cols-2">
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${id}-agg`}>Aggregate</Label>
        <select
          id={`${id}-agg`}
          className={selectClass}
          value={measure.agg}
          onChange={(event) => {
            const agg = AGGS.find((item) => item === event.target.value);
            if (agg) {
              onChange({ ...measure, agg, field: defaultField(source, agg), mapKey: "" });
            }
          }}
        >
          {AGGS.map((agg) => (
            <option key={agg} value={agg}>
              {AGG_LABELS[agg]}
            </option>
          ))}
        </select>
      </div>
      {fields.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${id}-field`}>Field</Label>
          <select
            id={`${id}-field`}
            className={selectClass}
            value={measure.field}
            disabled={measure.mapKey.trim() !== ""}
            onChange={(event) => onChange({ ...measure, field: event.target.value })}
          >
            <option value="">Choose a field</option>
            {fields.map((field) => (
              <option key={field} value={field}>
                {field}
              </option>
            ))}
          </select>
        </div>
      ) : null}
      {keyed ? (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor={`${id}-key`}>Map key</Label>
          <Input
            id={`${id}-key`}
            placeholder={`${mapColumns(source)[0]}['key']`}
            value={measure.mapKey}
            onChange={(event) => onChange({ ...measure, mapKey: event.target.value })}
          />
        </div>
      ) : null}
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${id}-where`}>
          {measure.agg === "rate" ? "Condition (required)" : "Condition (optional)"}
        </Label>
        <Input
          id={`${id}-where`}
          placeholder="status = 'error'"
          value={measure.where}
          onChange={(event) => onChange({ ...measure, where: event.target.value })}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor={`${id}-label`}>Label (optional)</Label>
        <Input
          id={`${id}-label`}
          maxLength={60}
          value={measure.label}
          onChange={(event) => onChange({ ...measure, label: event.target.value })}
        />
      </div>
      {canRemove ? (
        <div className="flex items-end">
          <Button type="button" variant="ghost" size="sm" onClick={onRemove}>
            Remove measure {position + 1}
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/** The options the chart type has, and only those (spec 04 § Dashboards). */
function ChartOptions({
  form,
  update,
}: {
  form: WidgetForm;
  update: (patch: Partial<WidgetForm>) => void;
}) {
  const options = optionsOf(form.type);
  if (!options.stacked && !options.split && !options.fadeBelow && !options.share) {
    return null;
  }
  return (
    <div className="flex flex-col gap-3">
      {options.stacked ? (
        <CheckField
          id="widget-stacked"
          checked={form.stacked}
          onChange={(value) => update({ stacked: value })}
        >
          Stack the series
        </CheckField>
      ) : null}
      {options.split ? (
        <CheckField
          id="widget-split"
          checked={form.split}
          onChange={(value) => update({ split: value })}
        >
          One small chart per group (needs a breakdown of at most 6)
        </CheckField>
      ) : null}
      {options.fadeBelow ? (
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="widget-fade">Fade buckets with fewer rows than (1 to 1000)</Label>
          <Input
            id="widget-fade"
            type="number"
            min={1}
            max={1000}
            value={form.fadeBelow}
            onChange={(event) => update({ fadeBelow: event.target.value })}
          />
        </div>
      ) : null}
      {options.share ? (
        <CheckField
          id="widget-share"
          checked={form.share}
          onChange={(value) => update({ share: value })}
        >
          Show each row's share of the first measure
        </CheckField>
      ) : null}
    </div>
  );
}

/**
 * The server's rules for one widget in its place in the dashboard. Issues about the dashboard
 * as a whole (its size, its id count) are left to the save.
 */
function issuesOf(draft: Definition, index: number, candidate: WidgetModel) {
  const widgets = [...draft.widgets];
  widgets[index] = candidate;
  return validateDefinition({ widgets }).filter((issue) => issueWidgetIndex(issue.field) === index);
}

/** The value, `delay` milliseconds after it last changed. */
function useDebounced<T>(value: T, delay: number): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(value), delay);
    return () => window.clearTimeout(timer);
  }, [value, delay]);
  return settled;
}

/** The heading prompt: a title of 1 to 100 characters. */
export function HeadingDialog({
  open,
  onOpenChange,
  initial,
  onApply,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initial: string;
  onApply: (title: string) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <HeadingForm
          initial={initial}
          onCancel={() => onOpenChange(false)}
          onApply={(title) => {
            onApply(title);
            onOpenChange(false);
          }}
        />
      </DialogContent>
    </Dialog>
  );
}

function HeadingForm({
  initial,
  onCancel,
  onApply,
}: {
  initial: string;
  onCancel: () => void;
  onApply: (title: string) => void;
}) {
  const [title, setTitle] = useState(initial);
  const length = Array.from(title.trim()).length;
  const valid = length >= 1 && length <= MAX_TITLE_CHARS;
  return (
    <form
      className="contents"
      onSubmit={(event) => {
        event.preventDefault();
        if (valid) {
          onApply(title.trim());
        }
      }}
    >
      <DialogHeader>
        <DialogTitle>{initial === "" ? "Add heading" : "Edit heading"}</DialogTitle>
        <DialogDescription>A heading spans the full width of the dashboard.</DialogDescription>
      </DialogHeader>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="heading-title">Heading</Label>
        <Input
          id="heading-title"
          value={title}
          aria-invalid={!valid}
          onChange={(event) => setTitle(event.target.value)}
        />
        {!valid ? (
          <p className="text-caption text-destructive">A heading needs 1 to 100 characters.</p>
        ) : null}
      </div>
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" disabled={!valid}>
          Apply
        </Button>
      </DialogFooter>
    </form>
  );
}
