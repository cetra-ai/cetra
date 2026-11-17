import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorQuery, useEvaluatorPages } from "@/api/queries-evaluation";
import { onlineRuleKey, onlineRuleStatsKey, onlineRulesKey } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import {
  defaultMappingFor,
  type MappingRow,
  MappingTable,
  mappingObject,
  newMappingRow,
} from "@/components/evaluators/mapping-table";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { MatchCounter } from "@/components/online-rules/match-counter";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { canManageEvaluation } from "@/lib/roles";

type Problem = Schema<"Problem">;
type Rule = Schema<"OnlineRule">;
type Target = Schema<"RuleTarget">;
type Period = Schema<"BudgetPeriod">;
type Evaluator = Schema<"Evaluator">;
type Errors = Record<string, string>;

const NO_PERMISSION = "Only members can edit online rules";
const PLAN_HINT =
  "A paused rule still counts as active: disable or delete a rule to free the slot.";
const KIND_LABELS: Record<Evaluator["kind"], string> = {
  llm_judge: "LLM judge",
  check: "Check",
  webhook: "Webhook",
};
const TARGET_LABELS: Record<Target, string> = { trace: "Traces", span: "Spans" };
const PERIOD_LABELS: Record<Period, string> = { day: "Per day", month: "Per month" };

/** The form's problem at the top of the page: a plan limit, a conflict or a field the form does not show. */
interface FormProblem {
  message: string;
  hint?: string;
  /** A plan limit: the alert also links to the upgrade page (D-P-15). */
  upgrade?: boolean;
}

/** Every value the form edits. The request is built from it on save. */
interface FormValues {
  name: string;
  evaluatorId: string;
  target: Target;
  filter: string;
  mapping: MappingRow[];
  /** The sample rate as a percent, "25" for 0.25. */
  samplePercent: string;
  limitSpend: boolean;
  /** The budget in USD, as typed. */
  budget: string;
  period: Period;
  enabled: boolean;
}

function percentText(rate: number): string {
  return String(Number((rate * 100).toFixed(4)));
}

function initialValues(rule: Rule | undefined): FormValues {
  if (rule === undefined) {
    return {
      name: "",
      evaluatorId: "",
      target: "trace",
      filter: "",
      mapping: [],
      samplePercent: "100",
      limitSpend: false,
      budget: "",
      period: "day",
      enabled: true,
    };
  }
  return {
    name: rule.name,
    evaluatorId: rule.evaluator_id,
    target: rule.target,
    filter: rule.filter ?? "",
    mapping: Object.entries(rule.variable_mapping).map(([variable, expression]) =>
      newMappingRow(variable, expression),
    ),
    samplePercent: percentText(rule.sample_rate),
    limitSpend: rule.budget_usd !== undefined,
    budget: rule.budget_usd !== undefined ? String(rule.budget_usd) : "",
    period: rule.budget_period ?? "day",
    enabled: rule.enabled,
  };
}

/** The rate a percent stands for, or `undefined` when it is not above 0 and at most 100. */
function sampleRateOf(percent: string): number | undefined {
  const value = Number(percent);
  if (percent.trim().length === 0 || !Number.isFinite(value) || value <= 0 || value > 100) {
    return undefined;
  }
  return Number((value / 100).toPrecision(12));
}

/** The client errors that block a save, keyed by the request field they belong to. */
function problemsOf(values: FormValues, judge: boolean): Errors {
  const errors: Errors = {};
  if (values.name.trim().length === 0) {
    errors.name = "Enter a name.";
  }
  if (values.evaluatorId.length === 0) {
    errors.evaluator_id = "Choose an evaluator.";
  }
  if (sampleRateOf(values.samplePercent) === undefined) {
    errors.sample_rate = "Enter a percent above 0 and at most 100.";
  }
  if (judge && values.limitSpend) {
    const amount = Number(values.budget);
    if (values.budget.trim().length === 0 || !Number.isFinite(amount) || amount <= 0) {
      errors.budget_usd = "Enter an amount above 0.";
    }
  }
  return errors;
}

function createBodyOf(
  values: FormValues,
  judge: boolean,
  sample: number,
): Schema<"CreateOnlineRuleRequest"> {
  const filter = values.filter.trim();
  return {
    name: values.name.trim(),
    evaluator_id: values.evaluatorId,
    target: values.target,
    variable_mapping: mappingObject(values.mapping),
    sample_rate: sample,
    enabled: values.enabled,
    ...(filter.length > 0 ? { filter } : {}),
    ...(judge && values.limitSpend
      ? { budget_usd: Number(values.budget), budget_period: values.period }
      : {}),
  };
}

function sameMapping(a: Record<string, string>, b: Record<string, string>): boolean {
  const keys = Object.keys(a);
  return keys.length === Object.keys(b).length && keys.every((key) => a[key] === b[key]);
}

/** Only the fields that differ from the saved rule. Clearing the budget is `clear_budget`. */
function patchBodyOf(
  values: FormValues,
  rule: Rule,
  judge: boolean,
  sample: number,
): Schema<"UpdateOnlineRuleRequest"> {
  const body: Schema<"UpdateOnlineRuleRequest"> = {};
  const name = values.name.trim();
  if (name !== rule.name) {
    body.name = name;
  }
  const filter = values.filter.trim();
  if (filter !== (rule.filter ?? "")) {
    body.filter = filter;
  }
  const mapping = mappingObject(values.mapping);
  if (!sameMapping(mapping, rule.variable_mapping)) {
    body.variable_mapping = mapping;
  }
  if (sample !== rule.sample_rate) {
    body.sample_rate = sample;
  }
  if (values.enabled !== rule.enabled) {
    body.enabled = values.enabled;
  }
  if (judge) {
    if (values.limitSpend) {
      const amount = Number(values.budget);
      if (amount !== rule.budget_usd || values.period !== rule.budget_period) {
        body.budget_usd = amount;
        body.budget_period = values.period;
      }
    } else if (rule.budget_usd !== undefined) {
      body.clear_budget = true;
    }
  }
  return body;
}

/** The message of a `invalid_filter` problem, with its position when the server gives one. */
function filterMessageOf(problem: Problem): string {
  const field = problem.errors?.find((item) => item.field === "filter");
  if (field === undefined) {
    return problem.detail ?? problem.title;
  }
  if (field.position === undefined) {
    return field.message;
  }
  return `${field.message} (line ${field.position.line}, column ${field.position.column})`;
}

/** Fields of a request that the form shows. Anything else is a form-level message. */
const SHOWN_FIELDS = new Set([
  "name",
  "evaluator_id",
  "target",
  "filter",
  "sample_rate",
  "budget_usd",
  "budget_period",
  "variable_mapping",
]);

/** Where a save's error goes: under the control that shows it, or the form-level alert. */
function placeErrors(caught: unknown): { placed: Errors; unplaced: string[] } {
  const problem = problemOf(caught);
  const placed: Errors = {};
  const unplaced: string[] = [];
  if (problem?.code === "invalid_filter") {
    placed.filter = filterMessageOf(problem);
    return { placed, unplaced };
  }
  if (problem?.status === 409 && problem.detail !== undefined && /name/i.test(problem.detail)) {
    placed.name = problem.detail;
  }
  for (const item of problem?.errors ?? []) {
    const shown = SHOWN_FIELDS.has(item.field) || item.field.startsWith("variable_mapping.");
    if (shown) {
      placed[item.field] ??= item.message;
    } else {
      unplaced.push(item.message);
    }
  }
  return { placed, unplaced };
}

/** The label of a rule's state in the list and the editor. */
export function ruleStatusLabel(rule: Pick<Rule, "enabled" | "paused_reason">): string {
  if (!rule.enabled) {
    return "Disabled";
  }
  switch (rule.paused_reason) {
    case "budget":
      return "Paused: budget";
    case "errors":
      return "Paused: errors";
    case "connection":
      return "Paused: connection";
    case undefined:
      return "Active";
  }
}

function SectionError({ message }: { message: string | undefined }) {
  if (!message) {
    return null;
  }
  return (
    <p role="alert" className="text-caption text-destructive">
      {message}
    </p>
  );
}

export function RuleForm({ org, project, rule }: { org: string; project: string; rule?: Rule }) {
  const { data: projectData } = useProject();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const ref = projectRef(org, project);
  const canEdit = canManageEvaluation(projectData.role);
  const saved = rule !== undefined;
  const [values, setValues] = useState<FormValues>(() => initialValues(rule));
  const [errors, setErrors] = useState<Errors>({});
  const [formError, setFormError] = useState<FormProblem | undefined>();
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const evaluators = useEvaluatorPages(org, project);
  const evaluatorItems = evaluators.data?.pages.flatMap((page) => page.data);
  // A saved rule's evaluator may be past the first page of the list: fetch it by id then.
  const savedEvaluatorId = rule?.evaluator_id ?? "";
  const fetchMissing =
    saved &&
    evaluatorItems !== undefined &&
    !evaluatorItems.some((item) => item.id === savedEvaluatorId);
  const missing = useQuery({
    ...evaluatorQuery(org, project, savedEvaluatorId),
    enabled: fetchMissing,
  });
  const create = $api.useMutation("post", "/api/v1/projects/{project}/online-rules", {
    gcTime: 0,
  });
  const patch = $api.useMutation("patch", "/api/v1/projects/{project}/online-rules/{id}", {
    gcTime: 0,
  });
  const pending = create.isPending || patch.isPending;
  const locked = pending || !canEdit;

  if (evaluators.error) {
    throw evaluators.error;
  }
  if (missing.error) {
    throw missing.error;
  }
  if (evaluators.isPending || (fetchMissing && missing.isPending)) {
    return <Skeleton className="h-64 w-full" />;
  }

  const evaluatorList =
    fetchMissing && missing.data
      ? [...(evaluatorItems ?? []), missing.data]
      : (evaluatorItems ?? []);
  const evaluator = evaluatorList.find((item) => item.id === values.evaluatorId);
  const judge = evaluator?.kind === "llm_judge";

  function set(change: Partial<FormValues>) {
    setValues((current) => ({ ...current, ...change }));
  }

  // On create, a new evaluator or target refills the mapping with that evaluator's usual variables.
  function pickEvaluator(id: string) {
    const picked = evaluatorList.find((item) => item.id === id);
    set({
      evaluatorId: id,
      ...(saved || picked === undefined
        ? {}
        : { mapping: defaultMappingFor(picked, values.target) }),
    });
  }

  function pickTarget(target: Target) {
    set({
      target,
      ...(saved || evaluator === undefined
        ? {}
        : { mapping: defaultMappingFor(evaluator, target) }),
    });
  }

  function onError(caught: unknown) {
    const problem = problemOf(caught);
    if (problem?.code === "plan_limit") {
      setErrors({});
      setFormError({ message: problem.detail ?? problem.title, hint: PLAN_HINT, upgrade: true });
      return;
    }
    const { placed, unplaced } = placeErrors(caught);
    setErrors(placed);
    if (unplaced.length > 0) {
      setFormError({ message: unplaced.join(" ") });
    } else if (Object.keys(placed).length === 0) {
      setFormError({ message: problemMessage(caught, "Could not save the rule.") });
    } else {
      setFormError(undefined);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    const problems = problemsOf(values, judge);
    if (Object.keys(problems).length > 0) {
      setErrors(problems);
      return;
    }
    setErrors({});
    const sample = sampleRateOf(values.samplePercent);
    if (sample === undefined) {
      return;
    }
    if (rule === undefined) {
      create.mutate(
        { params: { path: { project: ref } }, body: createBodyOf(values, judge, sample) },
        {
          onSuccess: (created) => {
            void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
            void queryClient.invalidateQueries({ queryKey: onlineRuleStatsKey(org, project) });
            void navigate({
              to: "/$org/$project/online-rules/$ruleId",
              params: { org, project, ruleId: created.id },
            });
          },
          onError,
        },
      );
      return;
    }
    const body = patchBodyOf(values, rule, judge, sample);
    if (Object.keys(body).length === 0) {
      void navigate({
        to: "/$org/$project/online-rules/$ruleId",
        params: { org, project, ruleId: rule.id },
      });
      return;
    }
    patch.mutate(
      {
        params: { path: { project: ref, id: rule.id } },
        body,
      },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: onlineRulesKey(org, project) });
          void queryClient.invalidateQueries({ queryKey: onlineRuleStatsKey(org, project) });
          void queryClient.invalidateQueries({ queryKey: onlineRuleKey(org, project, rule.id) });
          void navigate({
            to: "/$org/$project/online-rules/$ruleId",
            params: { org, project, ruleId: rule.id },
          });
        },
        onError,
      },
    );
  }

  const filterHelp = `Filter DSL over the ${values.target === "span" ? "spans" : "traces"} view, for example \`status = 'error'\`. Empty matches everything.`;
  const sampleHelp = `Percent of the matching ${values.target === "span" ? "spans" : "traces"} to evaluate, above 0 and at most 100.`;

  return (
    <form noValidate className="flex flex-col gap-6" onSubmit={onSubmit}>
      <fieldset disabled={locked} className="flex min-w-0 flex-col gap-6">
        <TextField
          id="rule-name"
          label="Name"
          value={values.name}
          maxLength={128}
          error={errors.name}
          onChange={(name) => {
            set({ name });
          }}
        />

        <div className="flex flex-col gap-2">
          <Label htmlFor="rule-evaluator">Evaluator</Label>
          <Select value={values.evaluatorId} onValueChange={pickEvaluator} disabled={saved}>
            <SelectTrigger
              id="rule-evaluator"
              className="w-fit"
              aria-invalid={errors.evaluator_id ? true : undefined}
              aria-describedby={saved ? "rule-evaluator-help" : undefined}
            >
              <SelectValue placeholder="Choose an evaluator" />
            </SelectTrigger>
            <SelectContent>
              {evaluatorList.map((item) => (
                <SelectItem key={item.id} value={item.id}>
                  {`${item.name} (${KIND_LABELS[item.kind]})`}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {saved ? (
            <p id="rule-evaluator-help" className="text-caption text-muted-foreground">
              The evaluator cannot change after the rule is created.
            </p>
          ) : null}
          <SectionError message={errors.evaluator_id} />
        </div>

        <div className="flex flex-col gap-2">
          <span className="text-body leading-none font-medium">Target</span>
          <RadioGroup
            aria-label="Target"
            value={values.target}
            disabled={saved}
            onValueChange={(next) => {
              pickTarget(next === "span" ? "span" : "trace");
            }}
            className="flex-row gap-4"
          >
            {(["trace", "span"] as const).map((item) => (
              <div key={item} className="flex items-center gap-2">
                <RadioGroupItem value={item} id={`rule-target-${item}`} />
                <Label htmlFor={`rule-target-${item}`}>{TARGET_LABELS[item]}</Label>
              </div>
            ))}
          </RadioGroup>
          {saved ? (
            <p className="text-caption text-muted-foreground">
              The target cannot change after the rule is created.
            </p>
          ) : null}
          <SectionError message={errors.target} />
        </div>

        <div className="flex flex-col gap-2">
          <Label htmlFor="rule-filter">Filter</Label>
          <Textarea
            id="rule-filter"
            className="font-mono"
            rows={3}
            value={values.filter}
            aria-invalid={errors.filter ? true : undefined}
            aria-describedby="rule-filter-help"
            onChange={(event) => {
              set({ filter: event.target.value });
            }}
          />
          <p id="rule-filter-help" className="text-caption text-muted-foreground">
            {filterHelp}
          </p>
          <SectionError message={errors.filter} />
          {canEdit ? (
            <MatchCounter
              org={org}
              project={project}
              target={values.target}
              filter={values.filter}
              sampleRate={sampleRateOf(values.samplePercent) ?? 1}
            />
          ) : null}
        </div>

        <div className="flex flex-col gap-2">
          <MappingTable
            rows={values.mapping}
            onChange={(mapping) => {
              set({ mapping });
            }}
            errorFor={(variable) => errors[`variable_mapping.${variable}`]}
          />
          <SectionError message={errors.variable_mapping} />
        </div>

        <div className="flex flex-col gap-2">
          <Label htmlFor="rule-sample">Sample rate (%)</Label>
          <Input
            id="rule-sample"
            name="rule-sample"
            type="number"
            min={0}
            max={100}
            step={0.1}
            value={values.samplePercent}
            aria-invalid={errors.sample_rate ? true : undefined}
            aria-describedby="rule-sample-help"
            onChange={(event) => {
              set({ samplePercent: event.target.value });
            }}
          />
          <p id="rule-sample-help" className="text-caption text-muted-foreground">
            {sampleHelp}
          </p>
          <SectionError message={errors.sample_rate} />
        </div>

        <section aria-label="Budget" className="flex flex-col gap-4">
          {judge ? (
            <>
              <div className="flex items-center gap-2">
                <Checkbox
                  id="rule-limit-spend"
                  checked={values.limitSpend}
                  onCheckedChange={(checked) => {
                    set({ limitSpend: checked === true });
                  }}
                />
                <Label htmlFor="rule-limit-spend">Limit spend</Label>
              </div>
              {values.limitSpend ? (
                <div className="flex flex-col gap-4 sm:flex-row">
                  <div className="flex flex-col gap-2">
                    <TextField
                      id="rule-budget"
                      type="number"
                      label="Amount (USD)"
                      value={values.budget}
                      error={errors.budget_usd}
                      onChange={(budget) => {
                        set({ budget });
                      }}
                    />
                    {errors.budget_usd?.includes("no price") ? (
                      <Link
                        to="/$org/$project/settings/models"
                        params={{ org, project }}
                        className="text-body underline"
                      >
                        Add a model price
                      </Link>
                    ) : null}
                  </div>
                  <div className="flex flex-col gap-2">
                    <Label htmlFor="rule-budget-period">Period</Label>
                    <Select
                      value={values.period}
                      onValueChange={(next) => {
                        set({ period: next === "month" ? "month" : "day" });
                      }}
                    >
                      <SelectTrigger id="rule-budget-period" className="w-fit">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {(["day", "month"] as const).map((item) => (
                          <SelectItem key={item} value={item}>
                            {PERIOD_LABELS[item]}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <SectionError message={errors.budget_period} />
                  </div>
                </div>
              ) : null}
            </>
          ) : evaluator === undefined ? null : (
            <p className="text-body text-muted-foreground">
              Budgets apply to LLM judge evaluators only.
            </p>
          )}
        </section>

        <div className="flex items-center gap-2">
          <Switch
            id="rule-enabled"
            size="sm"
            checked={values.enabled}
            onCheckedChange={(enabled) => {
              set({ enabled });
            }}
          />
          <Label htmlFor="rule-enabled">Enabled</Label>
        </div>

        {formError ? (
          <div role="alert" className="flex flex-col gap-1 text-body text-destructive">
            <p>{formError.message}</p>
            {formError.hint ? <p>{formError.hint}</p> : null}
            {formError.upgrade ? (
              <div>
                <UpgradeButton url={upgradeUrl} />
              </div>
            ) : null}
          </div>
        ) : null}
        <div className="flex items-center justify-end gap-2">
          {saved ? (
            <Button asChild variant="outline">
              <Link
                to="/$org/$project/online-rules/$ruleId"
                params={{ org, project, ruleId: rule.id }}
              >
                Cancel
              </Link>
            </Button>
          ) : null}
          <DisabledReason reason={canEdit ? undefined : NO_PERMISSION}>
            <SubmitButton pending={pending}>{saved ? "Save" : "Create rule"}</SubmitButton>
          </DisabledReason>
        </div>
      </fieldset>
    </form>
  );
}
