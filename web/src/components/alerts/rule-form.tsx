import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { alertRuleQuery, alertRulesQuery } from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { ChannelCheckboxes } from "@/components/alerts/channel-checkboxes";
import { IssueScopeFields } from "@/components/alerts/issue-scope-fields";
import { RulePreview } from "@/components/alerts/rule-preview";
import { type Option, SelectField } from "@/components/alerts/select-field";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  conditionChanged,
  conditionFromForm,
  DEFAULT_RULE_FORM,
  formFromRule,
  metricDefaults,
  type RuleFormValues,
  thresholdUnit,
} from "@/lib/alerts";

type Comparator = Schema<"Comparator">;
type Window = Schema<"AlertWindow">;

const METRIC_OPTIONS: Option<Schema<"AlertMetric">>[] = [
  { value: "error_rate", label: "Error rate" },
  { value: "cost", label: "Cost" },
  { value: "latency_p95", label: "p95 latency" },
  { value: "score", label: "Score" },
  { value: "issue_occurrences", label: "Issue occurrences" },
];
const TARGET_OPTIONS: Option<Schema<"LatencyTarget">>[] = [
  { value: "trace", label: "Whole traces" },
  { value: "llm_span", label: "LLM spans" },
];
const AGGREGATION_OPTIONS: Option<Schema<"ScoreAggregation">>[] = [
  { value: "avg", label: "Average" },
  { value: "pass_rate", label: "Pass rate" },
];
const WINDOW_OPTIONS: Option<Window>[] = [
  { value: "1m", label: "1 minute" },
  { value: "5m", label: "5 minutes" },
  { value: "15m", label: "15 minutes" },
  { value: "1h", label: "1 hour" },
  { value: "6h", label: "6 hours" },
  { value: "24h", label: "24 hours" },
];
/** Issue occurrences need a window of 15 minutes or more (D-IC-20). */
const ISSUE_WINDOW_OPTIONS = WINDOW_OPTIONS.filter(
  (option) => option.value !== "1m" && option.value !== "5m",
);
const COMPARATOR_OPTIONS: Option<Comparator>[] = [
  { value: "gt", label: "Above" },
  { value: "gte", label: "At or above" },
  { value: "lt", label: "Below" },
  { value: "lte", label: "At or below" },
];

/** The field names of the server's `400` that the form shows; others fall to `formError`. */
const FORM_FIELDS = [
  "name",
  "filter",
  "condition.filter",
  "condition.threshold",
  "condition.min_count",
  "condition.score_name",
  "condition.issue_id",
  "condition.source_id",
  "channel_ids",
  "cooldown_s",
] as const;

export function RuleForm(props: {
  org: string;
  project: string;
  /** Absent: create. */
  rule?: Schema<"AlertRule">;
  /** Create only: the values the new rule starts with, over the defaults. */
  initial?: Partial<RuleFormValues>;
  /** Called with the saved rule. */
  onSaved: (rule: Schema<"AlertRule">) => void;
  onCancel: () => void;
}) {
  const { org, project, rule, initial, onSaved, onCancel } = props;
  const queryClient = useQueryClient();
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const create = $api.useMutation("post", "/api/v1/projects/{project}/alert-rules");
  const update = $api.useMutation("patch", "/api/v1/projects/{project}/alert-rules/{id}");
  const [form, setForm] = useState<RuleFormValues>(() =>
    rule ? formFromRule(rule) : { ...DEFAULT_RULE_FORM, ...initial },
  );
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [formError, setFormError] = useState<string | undefined>();

  const pending = create.isPending || update.isPending;
  const planLimited = problemOf(create.error ?? update.error)?.code === "plan_limit";
  const resetsState = rule !== undefined && conditionChanged(formFromRule(rule), form);

  function patchForm(patch: Partial<RuleFormValues>) {
    setForm((current) => ({ ...current, ...patch }));
  }

  function changeMetric(metric: Schema<"AlertMetric">) {
    // Issue occurrences have no 1- or 5-minute window: such a window moves to one hour.
    const nextWindow: Window =
      metric === "issue_occurrences" && (form.window === "1m" || form.window === "5m")
        ? "1h"
        : form.window;
    patchForm({ metric, window: nextWindow, ...metricDefaults(metric, form.aggregation) });
  }

  function changeAggregation(aggregation: Schema<"ScoreAggregation">) {
    patchForm({ aggregation, threshold: aggregation === "pass_rate" ? "90" : "0.5" });
  }

  function toggleChannel(id: string, checked: boolean) {
    setForm((current) => {
      const others = current.channelIds.filter((channelId) => channelId !== id);
      return { ...current, channelIds: checked ? [...others, id] : others };
    });
  }

  function finish(saved: Schema<"AlertRule">) {
    void queryClient.invalidateQueries({ queryKey: alertRulesQuery(org, project).queryKey });
    queryClient.setQueryData(alertRuleQuery(org, project, saved.id).queryKey, saved);
    onSaved(saved);
  }

  function showError(caught: unknown) {
    const all = fieldErrors(caught);
    const mapped: Record<string, string> = {};
    for (const field of FORM_FIELDS) {
      if (all[field] !== undefined) {
        mapped[field] = all[field];
      }
    }
    setErrors(mapped);
    if (Object.keys(mapped).length > 0) {
      setFormError(undefined);
      return;
    }
    const problem = problemOf(caught);
    if (problem?.code === "plan_limit") {
      setFormError(problemMessage(caught, "Your plan does not allow another enabled alert rule."));
    } else if (problem?.status === 409) {
      setFormError(problemMessage(caught, "A rule with this name already exists."));
    } else if (problem?.status === 503) {
      setFormError("The server is busy. Try again.");
    } else {
      setFormError(problemMessage(caught, "Could not save the rule."));
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const name = form.name.trim();
    const built = conditionFromForm(form);
    const found: Record<string, string> = built.ok ? {} : { ...built.errors };
    if (!name) {
      found.name = "Enter a name.";
    }
    setErrors(found);
    setFormError(undefined);
    create.reset();
    update.reset();
    if (!built.ok || Object.keys(found).length > 0) {
      return;
    }

    if (!rule) {
      create.mutate(
        {
          params: { path: { project: projectRef(org, project) } },
          body: {
            name,
            condition: built.condition,
            channel_ids: form.channelIds,
            enabled: form.enabled,
            cooldown_s: built.cooldownS,
            notify_while_firing: form.notifyWhileFiring,
          },
        },
        { onSuccess: finish, onError: showError },
      );
      return;
    }

    const original = formFromRule(rule);
    const body: Schema<"UpdateAlertRuleRequest"> = {};
    if (name !== rule.name) {
      body.name = name;
    }
    if (conditionChanged(original, form)) {
      body.condition = built.condition;
    }
    if (!sameIds(form.channelIds, rule.channel_ids)) {
      body.channel_ids = form.channelIds;
    }
    if (form.notifyWhileFiring !== rule.notify_while_firing) {
      body.notify_while_firing = form.notifyWhileFiring;
    }
    if (
      form.notifyWhileFiring &&
      Number(form.cooldownMinutes.trim()) !== Number(original.cooldownMinutes)
    ) {
      body.cooldown_s = built.cooldownS;
    }
    if (Object.keys(body).length === 0) {
      onSaved(rule);
      return;
    }
    update.mutate(
      { params: { path: { project: projectRef(org, project), id: rule.id } }, body },
      { onSuccess: finish, onError: showError },
    );
  }

  const unit = thresholdUnit(form.metric, form.aggregation);
  const thresholdHint =
    unit === "%"
      ? "Percent."
      : unit === "USD"
        ? "USD."
        : unit === "ms"
          ? "Milliseconds."
          : unit === "occurrences"
            ? "Occurrences."
            : "Raw score.";
  const filterError = errors.filter ?? errors["condition.filter"];

  return (
    <form noValidate onSubmit={onSubmit} className="grid gap-8 lg:grid-cols-2">
      <div className="flex flex-col gap-6">
        <TextField
          id="rule-name"
          label="Name"
          value={form.name}
          maxLength={100}
          error={errors.name}
          disabled={pending}
          onChange={(name) => {
            patchForm({ name });
          }}
        />

        <fieldset className="flex flex-col gap-4">
          <legend className="text-body font-medium">What to measure</legend>
          <SelectField
            id="rule-metric"
            label="Metric"
            value={form.metric}
            options={METRIC_OPTIONS}
            disabled={pending}
            onChange={changeMetric}
          />
          {form.metric === "latency_p95" ? (
            <SelectField
              id="rule-target"
              label="Latency of"
              value={form.target}
              options={TARGET_OPTIONS}
              disabled={pending}
              onChange={(target) => {
                patchForm({ target });
              }}
            />
          ) : null}
          {form.metric === "score" ? (
            <>
              <TextField
                id="rule-score-name"
                label="Score name"
                value={form.scoreName}
                maxLength={100}
                error={errors["condition.score_name"]}
                disabled={pending}
                onChange={(scoreName) => {
                  patchForm({ scoreName });
                }}
              />
              <SelectField
                id="rule-aggregation"
                label="Aggregation"
                value={form.aggregation}
                options={AGGREGATION_OPTIONS}
                description="Pass rate counts true among boolean scores."
                disabled={pending}
                onChange={changeAggregation}
              />
            </>
          ) : null}
          {form.metric === "issue_occurrences" ? (
            <IssueScopeFields
              org={org}
              project={project}
              scope={form.issueScope}
              issueId={form.issueId}
              sourceId={form.sourceId}
              errors={errors}
              disabled={pending}
              onChange={patchForm}
            />
          ) : null}
          <SelectField
            id="rule-window"
            label="Window"
            value={form.window}
            options={form.metric === "issue_occurrences" ? ISSUE_WINDOW_OPTIONS : WINDOW_OPTIONS}
            description="Checked every minute for windows up to 15 minutes, every 5 minutes otherwise."
            disabled={pending}
            onChange={(window) => {
              patchForm({ window });
            }}
          />
        </fieldset>

        {form.metric !== "issue_occurrences" ? (
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="rule-filter">Filter</Label>
            <Textarea
              id="rule-filter"
              name="rule-filter"
              className="font-mono"
              rows={2}
              maxLength={4096}
              value={form.filter}
              disabled={pending}
              aria-invalid={filterError ? true : undefined}
              aria-describedby={
                filterError
                  ? "rule-filter-description rule-filter-error"
                  : "rule-filter-description"
              }
              onChange={(event) => {
                patchForm({ filter: event.target.value });
              }}
            />
            <p id="rule-filter-description" className="text-caption text-muted-foreground">
              A traces filter, e.g. environment = 'production'. Empty: every trace.
            </p>
            {filterError ? (
              <p id="rule-filter-error" className="text-caption text-destructive">
                {filterError}
              </p>
            ) : null}
          </div>
        ) : null}

        <fieldset className="flex flex-col gap-4">
          <legend className="text-body font-medium">Condition</legend>
          <SelectField
            id="rule-comparator"
            label="Comparator"
            value={form.comparator}
            options={COMPARATOR_OPTIONS}
            disabled={pending}
            onChange={(comparator) => {
              patchForm({ comparator });
            }}
          />
          <TextField
            id="rule-threshold"
            label="Threshold"
            value={form.threshold}
            description={thresholdHint}
            error={errors["condition.threshold"]}
            disabled={pending}
            onChange={(threshold) => {
              patchForm({ threshold });
            }}
          />
          {form.metric !== "cost" && form.metric !== "issue_occurrences" ? (
            <TextField
              id="rule-min-count"
              label="Minimum samples"
              value={form.minCount}
              description="Fewer samples in the window gives No data, which never fires."
              error={errors["condition.min_count"]}
              disabled={pending}
              onChange={(minCount) => {
                patchForm({ minCount });
              }}
            />
          ) : null}
        </fieldset>

        <fieldset
          className="flex flex-col gap-4"
          aria-describedby={errors.channel_ids ? "rule-channels-error" : undefined}
        >
          <legend className="text-body font-medium">Notifications</legend>
          <ChannelCheckboxes
            org={org}
            project={project}
            idPrefix="rule"
            selected={form.channelIds}
            onToggle={toggleChannel}
            disabled={pending}
            error={errors.channel_ids}
          />
          <div className="flex items-center gap-2">
            <Switch
              id="rule-notify"
              checked={form.notifyWhileFiring}
              disabled={pending}
              onCheckedChange={(notifyWhileFiring) => {
                patchForm({ notifyWhileFiring });
              }}
            />
            <Label htmlFor="rule-notify">Remind while firing</Label>
          </div>
          {form.notifyWhileFiring ? (
            <TextField
              id="rule-cooldown"
              label="Remind every (minutes)"
              value={form.cooldownMinutes}
              error={errors.cooldown_s}
              disabled={pending}
              onChange={(cooldownMinutes) => {
                patchForm({ cooldownMinutes });
              }}
            />
          ) : null}
        </fieldset>

        {rule === undefined ? (
          <div className="flex items-center gap-2">
            <Switch
              id="rule-enabled"
              checked={form.enabled}
              disabled={pending}
              onCheckedChange={(enabled) => {
                patchForm({ enabled });
              }}
            />
            <Label htmlFor="rule-enabled">Enabled</Label>
          </div>
        ) : null}
      </div>

      <RulePreview org={org} project={project} values={form} />

      <div className="flex flex-col gap-3 lg:col-span-2">
        {resetsState ? (
          <p className="text-body">Saving a new condition resets the rule to OK.</p>
        ) : null}
        <FormError message={formError} />
        {planLimited ? (
          <p className="text-caption">
            Upgrade your plan, disable another rule, or save this one disabled.
          </p>
        ) : null}
        {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="outline" disabled={pending} onClick={onCancel}>
            Cancel
          </Button>
          <SubmitButton pending={pending}>{rule ? "Save" : "Create rule"}</SubmitButton>
        </div>
      </div>
    </form>
  );
}

function sameIds(left: readonly string[], right: readonly string[]): boolean {
  const a = [...left].sort();
  const b = [...right].sort();
  return a.length === b.length && a.every((id, index) => id === b[index]);
}
