import { Link, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { useCreateIssueSource, usePatchIssueSource } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
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
type Errors = Record<string, string>;

const NO_PERMISSION = "Only members can manage issue sources";
const PLAN_HINT =
  "A disabled source counts for no plan limit: disable or delete another source to free the slot.";
const FILTER_HELP =
  "Error spans that match this filter become issues. `status = 'error'` is always added.";

/** Fields of a request that the form shows. Anything else is a form-level message. */
const SHOWN_FIELDS = new Set(["name", "filter", "enabled"]);

/** Every value the form edits. The request is built from it on save. */
interface FormValues {
  name: string;
  filter: string;
  enabled: boolean;
}

/** The form's problem at the top of the page: a plan limit or a field the form does not show. */
interface FormProblem {
  message: string;
  hint?: string;
  /** A plan limit: the alert also links to the upgrade page (D-P-15). */
  upgrade?: boolean;
}

function initialValues(source: Source | undefined): FormValues {
  if (source === undefined) {
    return { name: "", filter: "", enabled: true };
  }
  return { name: source.name, filter: source.filter ?? "", enabled: source.enabled };
}

/** The client errors that block a save, keyed by the request field they belong to. */
function problemsOf(values: FormValues): Errors {
  const errors: Errors = {};
  if (values.name.trim().length === 0) {
    errors.name = "Enter a name.";
  }
  return errors;
}

function createBodyOf(values: FormValues): Schema<"CreateIssueSourceRequest"> {
  const filter = values.filter.trim();
  return {
    name: values.name.trim(),
    kind: "error",
    enabled: values.enabled,
    ...(filter.length > 0 ? { filter } : {}),
  };
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
  return body;
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

/** The create and edit form of an issue source. The kind is fixed to errors. */
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
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const create = useCreateIssueSource(org, project);
  const patch = usePatchIssueSource(org, project);
  const pending = create.isPending || patch.isPending;
  const locked = pending || !canEdit;

  function set(change: Partial<FormValues>) {
    setValues((current) => ({ ...current, ...change }));
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
              <p className="text-body">Errors</p>
            </>
          ) : (
            <>
              <Label htmlFor="source-kind">Kind</Label>
              <Select value="error">
                <SelectTrigger id="source-kind" className="w-fit">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="error">Errors</SelectItem>
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
            {FILTER_HELP}
          </p>
          <SectionError message={errors.filter} />
        </div>

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
