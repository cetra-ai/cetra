import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import { useCreateIssueSource, usePatchIssueSource } from "@/api/queries-issues";
import { connectionsInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { KIND_LABELS } from "@/components/issue-sources/source-list";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { canManageEvaluation } from "@/lib/roles";

type Problem = Schema<"Problem">;
type Source = Schema<"IssueSource">;
type Kind = Schema<"IssueSourceKind">;
type Period = Schema<"BudgetPeriod">;
type Errors = Record<string, string>;

const NO_PERMISSION = "Only members can manage issue sources";
const PLAN_HINT =
  "A disabled source counts for no plan limit: disable or delete another source to free the slot.";
const ERROR_FILTER_HELP =
  "Error spans that match this filter become issues. `status = 'error'` is always added.";
const TRACE_FILTER_HELP = "Traces that match this filter are read. Empty matches every trace.";
const FIXED_HINT = "Fixed after create. Create a new source to change them.";
const NO_EMBEDDINGS = "No embeddings API";
const MAX_SCORE_NAMES = 8;

const KINDS: readonly Kind[] = ["error", "score", "feedback", "topic"];
const FEEDBACK_OPTIONS: readonly { value: string; label: string }[] = [
  { value: "end_user", label: "End users" },
  { value: "human", label: "Human reviewers" },
];
const PERIOD_LABELS: Record<Period, string> = { day: "Per day", month: "Per month" };

/** The form's problem at the top of the page: a plan limit or a field the form does not show. */
interface FormProblem {
  message: string;
  hint?: string;
  /** A plan limit: the alert also links to the upgrade page (D-P-15). */
  upgrade?: boolean;
}

/** Every value the form edits. The request is built from it on save. */
interface FormValues {
  name: string;
  kind: Kind;
  filter: string;
  enabled: boolean;
  scoreNames: string[];
  feedbackSources: string[];
  /** The sample rate as a percent, "25" for 0.25. */
  samplePercent: string;
  dailyCap: string;
  chatConnectionId: string;
  chatModel: string;
  embedConnectionId: string;
  embedModel: string;
  floor: string;
  minIssueSize: string;
  limitSpend: boolean;
  /** The budget in USD, as typed. */
  budget: string;
  period: Period;
}

function percentText(rate: number): string {
  return String(Number((rate * 100).toFixed(4)));
}

function kindOf(value: string): Kind {
  return KINDS.find((kind) => kind === value) ?? "error";
}

function initialValues(source: Source | undefined): FormValues {
  if (source === undefined) {
    return {
      name: "",
      kind: "error",
      filter: "",
      enabled: true,
      scoreNames: [],
      feedbackSources: ["end_user", "human"],
      samplePercent: "100",
      dailyCap: "500",
      chatConnectionId: "",
      chatModel: "",
      embedConnectionId: "",
      embedModel: "",
      floor: "0.8",
      minIssueSize: "5",
      limitSpend: false,
      budget: "",
      period: "day",
    };
  }
  return {
    name: source.name,
    kind: source.kind,
    filter: source.filter ?? "",
    enabled: source.enabled,
    scoreNames: source.score_names ?? [],
    feedbackSources: source.feedback_sources ?? [],
    samplePercent: percentText(source.sample_rate ?? 1),
    dailyCap: String(source.daily_cap ?? 500),
    chatConnectionId: source.chat_connection_id ?? "",
    chatModel: source.chat_model ?? "",
    embedConnectionId: source.embed_connection_id ?? "",
    embedModel: source.embed_model ?? "",
    floor: String(source.floor ?? 0.8),
    minIssueSize: String(source.min_issue_size ?? 5),
    limitSpend: source.budget_usd !== undefined,
    budget: source.budget_usd !== undefined ? String(source.budget_usd) : "",
    period: source.budget_period ?? "day",
  };
}

/** The number in `text` when it is finite and within `[min, max]`, or undefined. */
function numberIn(text: string, min: number, max: number): number | undefined {
  const value = Number(text);
  if (text.trim().length === 0 || !Number.isFinite(value) || value < min || value > max) {
    return undefined;
  }
  return value;
}

/** A whole number in `text` within `[min, max]`, or undefined. */
function wholeIn(text: string, min: number, max: number): number | undefined {
  const value = numberIn(text, min, max);
  return value !== undefined && Number.isInteger(value) ? value : undefined;
}

/** The rate a percent stands for. Only called after the percent passed `numberIn`. */
function rateOf(percent: string): number {
  return Number((Number(percent) / 100).toPrecision(12));
}

function isLlm(kind: Kind): boolean {
  return kind !== "error";
}

function cannotEmbed(adapter: string): boolean {
  return adapter === "anthropic";
}

/** The client errors that block a save, keyed by the request field they belong to. */
function problemsOf(values: FormValues): Errors {
  const errors: Errors = {};
  if (values.name.trim().length === 0) {
    errors.name = "Enter a name.";
  }
  if (!isLlm(values.kind)) {
    return errors;
  }
  if (values.kind !== "topic" && values.scoreNames.length === 0) {
    errors.score_names = "Choose at least one score name.";
  }
  if (values.kind === "feedback" && values.feedbackSources.length === 0) {
    errors.feedback_sources = "Choose at least one feedback source.";
  }
  if (numberIn(values.samplePercent, Number.MIN_VALUE, 100) === undefined) {
    errors.sample_rate = "Enter a percent above 0 and at most 100.";
  }
  if (wholeIn(values.dailyCap, 1, 10_000) === undefined) {
    errors.daily_cap = "Enter a whole number from 1 to 10000.";
  }
  if (numberIn(values.floor, 0.5, 0.99) === undefined) {
    errors.floor = "Enter a number from 0.5 to 0.99.";
  }
  if (wholeIn(values.minIssueSize, 2, 100) === undefined) {
    errors.min_issue_size = "Enter a whole number from 2 to 100.";
  }
  if (values.chatConnectionId.length === 0) {
    errors.chat_connection_id = "Choose a connection.";
  }
  if (values.chatModel.trim().length === 0) {
    errors.chat_model = "Enter a model.";
  }
  if (values.embedConnectionId.length === 0) {
    errors.embed_connection_id = "Choose a connection.";
  }
  if (values.embedModel.trim().length === 0) {
    errors.embed_model = "Enter a model.";
  }
  if (values.limitSpend) {
    const amount = Number(values.budget);
    if (values.budget.trim().length === 0 || !Number.isFinite(amount) || amount <= 0) {
      errors.budget_usd = "Enter an amount above 0.";
    }
  }
  return errors;
}

function createBodyOf(values: FormValues): Schema<"CreateIssueSourceRequest"> {
  const filter = values.filter.trim();
  const body: Schema<"CreateIssueSourceRequest"> = {
    name: values.name.trim(),
    kind: values.kind,
    enabled: values.enabled,
    ...(filter.length > 0 ? { filter } : {}),
  };
  if (!isLlm(values.kind)) {
    return body;
  }
  return {
    ...body,
    sample_rate: rateOf(values.samplePercent),
    daily_cap: Number(values.dailyCap),
    floor: Number(values.floor),
    min_issue_size: Number(values.minIssueSize),
    chat_connection_id: values.chatConnectionId,
    chat_model: values.chatModel.trim(),
    embed_connection_id: values.embedConnectionId,
    embed_model: values.embedModel.trim(),
    ...(values.kind !== "topic" ? { score_names: values.scoreNames } : {}),
    ...(values.kind === "feedback" ? { feedback_sources: values.feedbackSources } : {}),
    ...(values.limitSpend
      ? { budget_usd: Number(values.budget), budget_period: values.period }
      : {}),
  };
}

/** Order-insensitive equality of two lists of names. */
function sameNames(a: readonly string[], b: readonly string[]): boolean {
  return [...a].sort().join("\n") === [...b].sort().join("\n");
}

/** Only the fields that differ from the saved source. An empty filter clears it. */
function patchBodyOf(values: FormValues, source: Source): Schema<"UpdateIssueSourceRequest"> {
  const body: Schema<"UpdateIssueSourceRequest"> = {};
  const name = values.name.trim();
  if (name !== source.name) {
    body.name = name;
  }
  const filter = values.filter.trim();
  if (filter !== (source.filter ?? "")) {
    body.filter = filter;
  }
  if (values.enabled !== source.enabled) {
    body.enabled = values.enabled;
  }
  if (!isLlm(source.kind)) {
    return body;
  }
  const rate = rateOf(values.samplePercent);
  if (rate !== source.sample_rate) {
    body.sample_rate = rate;
  }
  const dailyCap = Number(values.dailyCap);
  if (dailyCap !== source.daily_cap) {
    body.daily_cap = dailyCap;
  }
  if (source.kind !== "topic" && !sameNames(values.scoreNames, source.score_names ?? [])) {
    body.score_names = values.scoreNames;
  }
  if (
    source.kind === "feedback" &&
    !sameNames(values.feedbackSources, source.feedback_sources ?? [])
  ) {
    body.feedback_sources = values.feedbackSources;
  }
  if (values.chatConnectionId !== (source.chat_connection_id ?? "")) {
    body.chat_connection_id = values.chatConnectionId;
  }
  const chatModel = values.chatModel.trim();
  if (chatModel !== (source.chat_model ?? "")) {
    body.chat_model = chatModel;
  }
  const floor = Number(values.floor);
  if (floor !== source.floor) {
    body.floor = floor;
  }
  const minIssueSize = Number(values.minIssueSize);
  if (minIssueSize !== source.min_issue_size) {
    body.min_issue_size = minIssueSize;
  }
  if (values.limitSpend) {
    const amount = Number(values.budget);
    if (amount !== source.budget_usd || values.period !== source.budget_period) {
      body.budget_usd = amount;
      body.budget_period = values.period;
    }
  } else if (source.budget_usd !== undefined) {
    body.clear_budget = true;
  }
  return body;
}

/** Adds or removes `item` from `list`, keeping the order of the other items. */
function toggled(list: readonly string[], item: string, on: boolean): string[] {
  const rest = list.filter((existing) => existing !== item);
  return on ? [...rest, item] : rest;
}

/** The message of an `invalid_filter` problem, with its position when the server gives one. */
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
  "filter",
  "enabled",
  "sample_rate",
  "daily_cap",
  "score_names",
  "feedback_sources",
  "chat_connection_id",
  "chat_model",
  "embed_connection_id",
  "embed_model",
  "floor",
  "min_issue_size",
  "budget_usd",
  "budget_period",
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
    if (SHOWN_FIELDS.has(item.field)) {
      placed[item.field] ??= item.message;
    } else {
      unplaced.push(item.message);
    }
  }
  return { placed, unplaced };
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

/** The create and edit form of an issue source. The kind is fixed after create. */
export function SourceForm({
  org,
  project,
  source,
}: {
  org: string;
  project: string;
  source?: Source;
}) {
  const { data: projectData } = useProject();
  const navigate = useNavigate();
  const ref = projectRef(org, project);
  const canEdit = canManageEvaluation(projectData.role);
  const saved = source !== undefined;
  const [values, setValues] = useState<FormValues>(() => initialValues(source));
  const [errors, setErrors] = useState<Errors>({});
  const [formError, setFormError] = useState<FormProblem | undefined>();
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [scoreDraft, setScoreDraft] = useState("");
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  const scores = useQuery(liveScoreDefinitionsQuery(org, project));
  const create = useCreateIssueSource(org, project);
  const patch = usePatchIssueSource(org, project);
  const pending = create.isPending || patch.isPending;
  const locked = pending || !canEdit;
  const connectionList = connections.data?.data ?? [];
  const llm = isLlm(values.kind);
  const showAdvanced =
    advancedOpen || errors.floor !== undefined || errors.min_issue_size !== undefined;
  const scoreOptions = [
    ...new Set([...(scores.data?.data ?? []).map((item) => item.name), ...values.scoreNames]),
  ];

  function set(change: Partial<FormValues>) {
    setValues((current) => ({ ...current, ...change }));
  }

  function connectionName(id: string): string {
    return connectionList.find((connection) => connection.id === id)?.name ?? "—";
  }

  // A new connection fills the model with its default when the model is still empty.
  function pickChat(id: string) {
    const connection = connectionList.find((item) => item.id === id);
    set({
      chatConnectionId: id,
      chatModel:
        values.chatModel.trim().length === 0 && connection?.default_model
          ? connection.default_model
          : values.chatModel,
    });
  }

  function addScoreName() {
    const name = scoreDraft.trim();
    if (name.length === 0) {
      return;
    }
    if (!values.scoreNames.includes(name)) {
      if (values.scoreNames.length >= MAX_SCORE_NAMES) {
        setErrors((current) => ({
          ...current,
          score_names: `Choose at most ${MAX_SCORE_NAMES} score names.`,
        }));
        return;
      }
      set({ scoreNames: [...values.scoreNames, name] });
    }
    setScoreDraft("");
  }

  function backToList() {
    void navigate({ to: "/$org/$project/issue-sources", params: { org, project } });
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
      setFormError({ message: problemMessage(caught, "Could not save the source.") });
    } else {
      setFormError(undefined);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    const problems = problemsOf(values);
    if (Object.keys(problems).length > 0) {
      setErrors(problems);
      return;
    }
    setErrors({});
    if (source === undefined) {
      create.mutate(
        { params: { path: { project: ref } }, body: createBodyOf(values) },
        { onSuccess: backToList, onError },
      );
      return;
    }
    const body = patchBodyOf(values, source);
    if (Object.keys(body).length === 0) {
      backToList();
      return;
    }
    patch.mutate(
      { params: { path: { project: ref, id: source.id } }, body },
      { onSuccess: backToList, onError },
    );
  }

  return (
    <form noValidate className="flex flex-col gap-6" onSubmit={onSubmit}>
      <fieldset disabled={locked} className="flex min-w-0 flex-col gap-6">
        <TextField
          id="source-name"
          label="Name"
          value={values.name}
          maxLength={128}
          error={errors.name}
          onChange={(name) => {
            set({ name });
          }}
        />

        <div className="flex flex-col gap-2">
          {saved ? (
            <>
              <span className="text-body leading-none font-medium">Kind</span>
              <p className="text-body">{KIND_LABELS[values.kind]}</p>
            </>
          ) : (
            <>
              <Label htmlFor="source-kind">Kind</Label>
              <Select
                value={values.kind}
                onValueChange={(next) => {
                  set({ kind: kindOf(next) });
                }}
              >
                <SelectTrigger id="source-kind" className="w-fit">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {KINDS.map((kind) => (
                    <SelectItem key={kind} value={kind}>
                      {KIND_LABELS[kind]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </>
          )}
        </div>

        <div className="flex flex-col gap-2">
          <Label htmlFor="source-filter">Filter</Label>
          <Textarea
            id="source-filter"
            className="font-mono"
            rows={3}
            value={values.filter}
            aria-invalid={errors.filter ? true : undefined}
            aria-describedby="source-filter-help"
            onChange={(event) => {
              set({ filter: event.target.value });
            }}
          />
          <p id="source-filter-help" className="text-caption text-muted-foreground">
            {llm ? TRACE_FILTER_HELP : ERROR_FILTER_HELP}
          </p>
          <SectionError message={errors.filter} />
        </div>

        {values.kind === "score" || values.kind === "feedback" ? (
          <div
            role="group"
            aria-labelledby="source-score-names-label"
            className="flex flex-col gap-2"
          >
            <span id="source-score-names-label" className="text-body leading-none font-medium">
              Score names
            </span>
            {scoreOptions.length === 0 ? (
              <p className="text-caption text-muted-foreground">
                No score definitions yet. Add a score name below.
              </p>
            ) : null}
            {scoreOptions.map((name, index) => (
              <div key={name} className="flex items-center gap-2">
                <Checkbox
                  id={`source-score-${index}`}
                  checked={values.scoreNames.includes(name)}
                  onCheckedChange={(checked) => {
                    set({ scoreNames: toggled(values.scoreNames, name, checked === true) });
                  }}
                />
                <Label htmlFor={`source-score-${index}`}>{name}</Label>
              </div>
            ))}
            <div className="flex flex-wrap items-center gap-2">
              <Input
                id="source-score-draft"
                aria-label="Another score name"
                placeholder="Another score name"
                className="w-64"
                value={scoreDraft}
                onChange={(event) => {
                  setScoreDraft(event.target.value);
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    addScoreName();
                  }
                }}
              />
              <Button type="button" variant="outline" onClick={addScoreName}>
                Add score name
              </Button>
            </div>
            <SectionError message={errors.score_names} />
          </div>
        ) : null}

        {values.kind === "feedback" ? (
          <div role="group" aria-labelledby="source-feedback-label" className="flex flex-col gap-2">
            <span id="source-feedback-label" className="text-body leading-none font-medium">
              Feedback sources
            </span>
            <div className="flex flex-row gap-4">
              {FEEDBACK_OPTIONS.map((option) => (
                <div key={option.value} className="flex items-center gap-2">
                  <Checkbox
                    id={`source-feedback-${option.value}`}
                    checked={values.feedbackSources.includes(option.value)}
                    onCheckedChange={(checked) => {
                      set({
                        feedbackSources: toggled(
                          values.feedbackSources,
                          option.value,
                          checked === true,
                        ),
                      });
                    }}
                  />
                  <Label htmlFor={`source-feedback-${option.value}`}>{option.label}</Label>
                </div>
              ))}
            </div>
            <SectionError message={errors.feedback_sources} />
          </div>
        ) : null}

        {llm ? (
          <section aria-label="Models" className="flex flex-col gap-6">
            <div className="flex flex-col gap-2">
              <Label htmlFor="source-chat-connection">Chat connection</Label>
              <Select value={values.chatConnectionId} onValueChange={pickChat}>
                <SelectTrigger
                  id="source-chat-connection"
                  className="w-fit"
                  aria-invalid={errors.chat_connection_id ? true : undefined}
                >
                  <SelectValue placeholder="Choose a connection" />
                </SelectTrigger>
                <SelectContent>
                  {connectionList.map((connection) => (
                    <SelectItem key={connection.id} value={connection.id}>
                      {connection.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {connections.isSuccess && connectionList.length === 0 ? (
                <p className="text-caption text-muted-foreground">
                  No connections. An org admin adds them in{" "}
                  <Link
                    to="/$org/settings/llm-connections"
                    params={{ org }}
                    className="underline underline-offset-4"
                  >
                    Settings › LLM connections
                  </Link>
                  .
                </p>
              ) : null}
              <SectionError message={errors.chat_connection_id} />
            </div>
            <TextField
              id="source-chat-model"
              label="Chat model"
              value={values.chatModel}
              error={errors.chat_model}
              onChange={(chatModel) => {
                set({ chatModel });
              }}
            />

            <div className="flex flex-col gap-2">
              <Label htmlFor="source-embed-connection">Embedding connection</Label>
              {saved ? (
                <p id="source-embed-connection" className="text-body">
                  {connectionName(values.embedConnectionId)}
                </p>
              ) : (
                <Select
                  value={values.embedConnectionId}
                  onValueChange={(embedConnectionId) => {
                    set({ embedConnectionId });
                  }}
                >
                  <SelectTrigger
                    id="source-embed-connection"
                    className="w-fit"
                    aria-invalid={errors.embed_connection_id ? true : undefined}
                  >
                    <SelectValue placeholder="Choose a connection" />
                  </SelectTrigger>
                  <SelectContent>
                    {connectionList.map((connection) => (
                      <SelectItem
                        key={connection.id}
                        value={connection.id}
                        disabled={cannotEmbed(connection.adapter)}
                      >
                        {cannotEmbed(connection.adapter)
                          ? `${connection.name} · ${NO_EMBEDDINGS}`
                          : connection.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              )}
              <SectionError message={errors.embed_connection_id} />
            </div>
            <TextField
              id="source-embed-model"
              label="Embedding model"
              value={values.embedModel}
              readOnly={saved}
              description={saved ? FIXED_HINT : undefined}
              error={errors.embed_model}
              onChange={(embedModel) => {
                set({ embedModel });
              }}
            />

            <TextField
              id="source-sample"
              label="Sample rate (%)"
              type="number"
              min={0}
              max={100}
              step={0.1}
              value={values.samplePercent}
              description="Percent of the matching traces or scores to read, above 0 and at most 100."
              error={errors.sample_rate}
              onChange={(samplePercent) => {
                set({ samplePercent });
              }}
            />
            <TextField
              id="source-daily-cap"
              label="Daily cap"
              type="number"
              min={1}
              max={10000}
              step={1}
              value={values.dailyCap}
              description="Most candidates admitted per UTC day, 1 to 10000."
              error={errors.daily_cap}
              onChange={(dailyCap) => {
                set({ dailyCap });
              }}
            />

            <section aria-label="Budget" className="flex flex-col gap-4">
              <div className="flex items-center gap-2">
                <Checkbox
                  id="source-limit-spend"
                  checked={values.limitSpend}
                  onCheckedChange={(checked) => {
                    set({ limitSpend: checked === true });
                  }}
                />
                <Label htmlFor="source-limit-spend">Limit spend</Label>
              </div>
              {values.limitSpend ? (
                <div className="flex flex-col gap-4 sm:flex-row">
                  <div className="flex flex-col gap-2">
                    <TextField
                      id="source-budget"
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
                    <Label htmlFor="source-budget-period">Period</Label>
                    <Select
                      value={values.period}
                      onValueChange={(next) => {
                        set({ period: next === "month" ? "month" : "day" });
                      }}
                    >
                      <SelectTrigger id="source-budget-period" className="w-fit">
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
            </section>

            <div className="flex flex-col gap-4">
              <Button
                type="button"
                variant="ghost"
                className="w-fit"
                aria-expanded={showAdvanced}
                aria-controls="source-advanced"
                onClick={() => {
                  setAdvancedOpen((open) => !open);
                }}
              >
                Advanced
              </Button>
              {showAdvanced ? (
                <div id="source-advanced" className="flex flex-col gap-6">
                  <TextField
                    id="source-floor"
                    label="Join floor"
                    type="number"
                    min={0.5}
                    max={0.99}
                    step={0.01}
                    value={values.floor}
                    description="Cosine similarity at which a candidate joins an issue, from 0.5 to 0.99. Default 0.80."
                    error={errors.floor}
                    onChange={(floor) => {
                      set({ floor });
                    }}
                  />
                  <TextField
                    id="source-min-size"
                    label="Minimum issue size"
                    type="number"
                    min={2}
                    max={100}
                    step={1}
                    value={values.minIssueSize}
                    description="Smallest group a refit makes into a new issue, from 2 to 100. Default 5."
                    error={errors.min_issue_size}
                    onChange={(minIssueSize) => {
                      set({ minIssueSize });
                    }}
                  />
                </div>
              ) : null}
            </div>
          </section>
        ) : null}

        <div className="flex items-center gap-2">
          <Switch
            id="source-enabled"
            size="sm"
            checked={values.enabled}
            onCheckedChange={(enabled) => {
              set({ enabled });
            }}
          />
          <Label htmlFor="source-enabled">Enabled</Label>
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
              <Link to="/$org/$project/issue-sources" params={{ org, project }}>
                Cancel
              </Link>
            </Button>
          ) : null}
          <DisabledReason reason={canEdit ? undefined : NO_PERMISSION}>
            <SubmitButton pending={pending}>{saved ? "Save" : "Create source"}</SubmitButton>
          </DisabledReason>
        </div>
      </fieldset>
    </form>
  );
}
