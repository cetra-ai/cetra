import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useId, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { meQuery } from "@/api/queries";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import {
  type AnnotationContext,
  type AnnotationTarget,
  type Draft,
  draftError,
  draftOf,
  invalidateScoreReads,
  isDirty,
  isSet,
  ownScores,
  rowErrors,
  scoreItem,
  scoreListQuery,
} from "@/lib/annotation";
import { canAnnotate } from "@/lib/roles";

type Score = Schema<"Score">;
type ScoreDefinition = Schema<"ScoreDefinition">;

/** The annotation form of one target: one control per live definition, the caller's own scores (D-I-37). */
export function AnnotationForm({
  target,
  context,
}: {
  target: AnnotationTarget;
  context: AnnotationContext;
}) {
  const { org, project, data } = useProject();
  const me = useQuery(meQuery);
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  const scores = useQuery(scoreListQuery(org, project, target));

  if (me.isPending || definitions.isPending || scores.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-4 w-2/3" />
        <Skeleton className="h-4 w-full" />
      </div>
    );
  }
  if (me.isError || definitions.isError || scores.isError) {
    const error = me.error ?? definitions.error ?? scores.error;
    return (
      <p className="text-sm">{problemOf(error)?.title ?? "Could not load the annotation form"}</p>
    );
  }
  if (!canAnnotate(data.role)) {
    return <p className="text-xs text-muted-foreground">Only members can annotate.</p>;
  }
  if (definitions.data.data.length === 0) {
    return (
      <div className="flex flex-col gap-1">
        <p className="text-xs text-muted-foreground">No score definitions yet.</p>
        <Link
          to="/$org/$project/settings/score-definitions"
          params={{ org, project }}
          className="w-fit text-xs underline underline-offset-4"
        >
          Add score definitions
        </Link>
      </div>
    );
  }
  return (
    <AnnotationRows
      definitions={definitions.data.data}
      own={ownScores(scores.data.data, target, me.data.user.id)}
      target={target}
      context={context}
    />
  );
}

/** The form's state and markup, once everything has loaded. The caller keys the form by target. */
function AnnotationRows({
  definitions,
  own,
  target,
  context,
}: {
  definitions: ScoreDefinition[];
  own: Map<string, Score>;
  target: AnnotationTarget;
  context: AnnotationContext;
}) {
  const { ref } = useProject();
  const queryClient = useQueryClient();
  const create = $api.useMutation("post", "/api/v1/projects/{project}/scores");
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/scores/{score_id}");
  const [edits, setEdits] = useState<Record<string, Draft>>({});
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({});
  const [success, setSuccess] = useState(false);
  const [formError, setFormError] = useState<string | undefined>();

  const rows = [...definitions].sort((left, right) => left.name.localeCompare(right.name));
  const savedOf = (definition: ScoreDefinition) => draftOf(own.get(definition.name), definition);
  const draftOfRow = (definition: ScoreDefinition) => edits[definition.id] ?? savedOf(definition);
  const changed = rows.filter((definition) => {
    const draft = draftOfRow(definition);
    return isSet(draft) && isDirty(draft, savedOf(definition));
  });
  const busy = create.isPending || remove.isPending;
  const canSave =
    changed.length > 0 &&
    changed.every((definition) => draftError(draftOfRow(definition), definition) === undefined) &&
    !busy;

  function change(definition: ScoreDefinition, draft: Draft) {
    setEdits((current) => ({ ...current, [definition.id]: draft }));
    setServerErrors((current) => without(current, definition.id));
    setSuccess(false);
    setFormError(undefined);
  }

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!canSave) {
      return;
    }
    const sent = changed;
    setSuccess(false);
    setFormError(undefined);
    try {
      await create.mutateAsync({
        params: { path: { project: ref } },
        body: {
          scores: sent.map((definition) =>
            scoreItem(definition, draftOfRow(definition), target, context),
          ),
        },
      });
    } catch (error) {
      const rowsError = rowErrors(
        error,
        sent.map((definition) => definition.id),
      );
      if (Object.keys(rowsError).length > 0) {
        setServerErrors(rowsError);
      } else {
        setFormError(problemMessage(error, "Could not save the annotations."));
      }
      return;
    }
    await invalidateScoreReads(queryClient);
    setEdits({});
    setServerErrors({});
    setSuccess(true);
  }

  async function clear(definition: ScoreDefinition) {
    const score = own.get(definition.name);
    if (score === undefined) {
      return;
    }
    setSuccess(false);
    setFormError(undefined);
    try {
      await remove.mutateAsync({
        params: { path: { project: ref, score_id: score.score_id } },
      });
    } catch (error) {
      setFormError(problemMessage(error, "Could not clear the score."));
      return;
    }
    await invalidateScoreReads(queryClient);
    setEdits((current) => without(current, definition.id));
    setServerErrors((current) => without(current, definition.id));
  }

  return (
    <form aria-label="Annotations" className="flex flex-col gap-3" onSubmit={save}>
      {rows.map((definition) => {
        const draft = draftOfRow(definition);
        return (
          <DefinitionRow
            key={definition.id}
            definition={definition}
            draft={draft}
            own={own.get(definition.name)}
            error={draftError(draft, definition) ?? serverErrors[definition.id]}
            disabled={busy}
            onChange={(next) => {
              change(definition, next);
            }}
            onClear={() => {
              void clear(definition);
            }}
          />
        );
      })}
      <FormError message={formError} />
      <FormSuccess message={success ? "Saved." : undefined} />
      <div>
        <Button type="submit" size="sm" disabled={!canSave}>
          Save
        </Button>
      </div>
    </form>
  );
}

function DefinitionRow({
  definition,
  draft,
  own,
  error,
  disabled,
  onChange,
  onClear,
}: {
  definition: ScoreDefinition;
  draft: Draft;
  own: Score | undefined;
  error: string | undefined;
  disabled: boolean;
  onChange: (draft: Draft) => void;
  onClear: () => void;
}) {
  const id = useId();
  const errorId = `${id}-error`;
  const invalid = error !== undefined;
  const describedBy = invalid ? errorId : undefined;
  const { name } = definition;

  return (
    <fieldset className="flex flex-col gap-1">
      <legend className="text-xs font-medium">{name}</legend>
      {definition.description ? (
        <p className="text-xs text-muted-foreground">{definition.description}</p>
      ) : null}
      {renderControl(definition, draft, { id, invalid, describedBy, disabled, onChange })}
      <div className="flex items-center gap-2">
        <Input
          aria-label={`${name} reason`}
          placeholder="Reason (optional)"
          value={draft.reason}
          disabled={disabled}
          onChange={(event) => {
            onChange({ ...draft, reason: event.target.value });
          }}
        />
        {own ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            aria-label={`Clear ${name}`}
            disabled={disabled}
            onClick={onClear}
          >
            Clear
          </Button>
        ) : null}
      </div>
      {invalid ? (
        <p id={errorId} className="text-xs text-destructive">
          {error}
        </p>
      ) : null}
    </fieldset>
  );
}

function renderControl(
  definition: ScoreDefinition,
  draft: Draft,
  control: {
    id: string;
    invalid: boolean;
    describedBy: string | undefined;
    disabled: boolean;
    onChange: (draft: Draft) => void;
  },
) {
  const { id, invalid, describedBy, disabled, onChange } = control;
  const { name } = definition;
  switch (definition.data_type) {
    case "numeric": {
      const range = rangeText(definition.min_value, definition.max_value);
      return (
        <div className="flex flex-col gap-0.5">
          <Input
            type="number"
            step="any"
            inputMode="decimal"
            aria-label={name}
            min={definition.min_value}
            max={definition.max_value}
            value={draft.value}
            aria-invalid={invalid}
            aria-describedby={describedBy}
            disabled={disabled}
            onChange={(event) => {
              onChange({ ...draft, value: event.target.value });
            }}
          />
          {range ? <span className="text-xs text-muted-foreground">{range}</span> : null}
        </div>
      );
    }
    case "boolean":
      return (
        <RadioGroup
          aria-label={name}
          value={draft.value}
          disabled={disabled}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onValueChange={(value) => {
            onChange({ ...draft, value });
          }}
          className="flex gap-3"
        >
          <div className="flex items-center gap-2">
            <RadioGroupItem value="true" id={`${id}-true`} />
            <Label htmlFor={`${id}-true`}>Yes</Label>
          </div>
          <div className="flex items-center gap-2">
            <RadioGroupItem value="false" id={`${id}-false`} />
            <Label htmlFor={`${id}-false`}>No</Label>
          </div>
        </RadioGroup>
      );
    case "categorical":
      return (
        <RadioGroup
          aria-label={name}
          value={draft.value}
          disabled={disabled}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onValueChange={(value) => {
            onChange({ ...draft, value });
          }}
          className="flex flex-wrap gap-3"
        >
          {definition.categories.map((category, index) => (
            <div key={category.label} className="flex items-center gap-2">
              <RadioGroupItem value={category.label} id={`${id}-${index}`} />
              <Label htmlFor={`${id}-${index}`}>{category.label}</Label>
            </div>
          ))}
        </RadioGroup>
      );
    case "text":
      return (
        <Textarea
          aria-label={name}
          rows={2}
          value={draft.value}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          disabled={disabled}
          onChange={(event) => {
            onChange({ ...draft, value: event.target.value });
          }}
        />
      );
  }
}

/** The range under a numeric control, worded as the score definitions page words it. */
function rangeText(min: number | undefined, max: number | undefined): string | undefined {
  if (min !== undefined && max !== undefined) {
    return `${min} – ${max}`;
  }
  if (min !== undefined) {
    return `≥ ${min}`;
  }
  if (max !== undefined) {
    return `≤ ${max}`;
  }
  return undefined;
}

function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([name]) => name !== key));
}
