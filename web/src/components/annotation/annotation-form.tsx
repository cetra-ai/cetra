import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useId, useImperativeHandle, useRef, useState } from "react";
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
  type AnnotationSaveHandle,
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

/**
 * The annotation form of one target: one control per live definition, the caller's own scores (D-I-37).
 * `definitionIds` restricts the rows to those live definitions, in that order. `saveRef` receives
 * an imperative save handle while the rows are shown. `onReady` fires once, after loading.
 */
export function AnnotationForm({
  target,
  context,
  definitionIds,
  saveRef,
  hideSave = false,
  onReady,
}: {
  target: AnnotationTarget;
  context: AnnotationContext;
  definitionIds?: readonly string[];
  saveRef?: { current: AnnotationSaveHandle | null };
  hideSave?: boolean;
  onReady?: () => void;
}) {
  const { org, project, data } = useProject();
  const me = useQuery(meQuery);
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  const scores = useQuery(scoreListQuery(org, project, target));
  const loaded = !(me.isPending || definitions.isPending || scores.isPending);
  const notified = useRef(false);
  useEffect(() => {
    if (loaded && !notified.current) {
      notified.current = true;
      onReady?.();
    }
  }, [loaded, onReady]);

  if (!loaded) {
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
      <p className="text-body">{problemOf(error)?.title ?? "Could not load the annotation form"}</p>
    );
  }
  if (!canAnnotate(data.role)) {
    return <p className="text-caption text-muted-foreground">Only members can annotate.</p>;
  }
  const rows = definitionRows(definitions.data.data, definitionIds);
  if (rows.length === 0) {
    if (definitionIds !== undefined) {
      return (
        <p className="text-caption text-muted-foreground">
          This queue's score definitions are all archived.
        </p>
      );
    }
    return (
      <div className="flex flex-col gap-1">
        <p className="text-caption text-muted-foreground">No score definitions yet.</p>
        <Link
          to="/$org/$project/settings/score-definitions"
          params={{ org, project }}
          className="w-fit text-caption underline underline-offset-4"
        >
          Add score definitions
        </Link>
      </div>
    );
  }
  return (
    <AnnotationRows
      definitions={rows}
      own={ownScores(scores.data.data, target, me.data.user.id)}
      target={target}
      context={context}
      saveRef={saveRef}
      hideSave={hideSave}
    />
  );
}

/**
 * The rows to show: the ids' live definitions in the ids' order (unknown and repeated ids dropped),
 * or every live definition sorted by name without ids.
 */
function definitionRows(
  live: readonly ScoreDefinition[],
  definitionIds: readonly string[] | undefined,
): ScoreDefinition[] {
  if (definitionIds === undefined) {
    return [...live].sort((left, right) => left.name.localeCompare(right.name));
  }
  const byId = new Map(live.map((definition) => [definition.id, definition]));
  return [...new Set(definitionIds)].flatMap((id) => {
    const definition = byId.get(id);
    return definition === undefined ? [] : [definition];
  });
}

/** The form's state and markup, once everything has loaded. The caller keys the form by target. */
function AnnotationRows({
  definitions,
  own,
  target,
  context,
  saveRef,
  hideSave,
}: {
  definitions: ScoreDefinition[];
  own: Map<string, Score>;
  target: AnnotationTarget;
  context: AnnotationContext;
  saveRef: { current: AnnotationSaveHandle | null } | undefined;
  hideSave: boolean;
}) {
  const { ref } = useProject();
  const queryClient = useQueryClient();
  const create = $api.useMutation("post", "/api/v1/projects/{project}/scores");
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/scores/{score_id}");
  const [edits, setEdits] = useState<Record<string, Draft>>({});
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({});
  const [success, setSuccess] = useState(false);
  const [formError, setFormError] = useState<string | undefined>();
  const [refreshing, setRefreshing] = useState(false);

  const savedOf = (definition: ScoreDefinition) => draftOf(own.get(definition.name), definition);
  const draftOfRow = (definition: ScoreDefinition) => edits[definition.id] ?? savedOf(definition);
  const changed = definitions.filter((definition) => {
    const draft = draftOfRow(definition);
    return isSet(draft) && isDirty(draft, savedOf(definition));
  });
  // Still busy while the score reads refresh, so the form keeps its fields locked until it shows the saved values.
  const busy = create.isPending || remove.isPending || refreshing;
  const allValid = changed.every(
    (definition) => draftError(draftOfRow(definition), definition) === undefined,
  );

  function change(definition: ScoreDefinition, draft: Draft) {
    if (busy) {
      return;
    }
    setEdits((current) => ({ ...current, [definition.id]: draft }));
    setServerErrors((current) => without(current, [definition.id]));
    setSuccess(false);
    setFormError(undefined);
  }

  async function refresh() {
    setRefreshing(true);
    try {
      await invalidateScoreReads(queryClient);
    } finally {
      setRefreshing(false);
    }
  }

  // Saves the changed rows. Resolves true when there was nothing to save or the save succeeded,
  // and false when the save was refused or failed (its errors are shown on the form).
  async function submit(): Promise<boolean> {
    if (busy) {
      return false;
    }
    if (changed.length === 0) {
      return true;
    }
    if (!allValid) {
      return false;
    }
    const sent = changed;
    const sentIds = sent.map((definition) => definition.id);
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
      const rowsError = rowErrors(error, sentIds);
      if (Object.keys(rowsError).length > 0) {
        setServerErrors(rowsError);
      } else {
        setFormError(problemMessage(error, "Could not save the annotations."));
      }
      return false;
    }
    await refresh();
    setEdits((current) => without(current, sentIds));
    setServerErrors((current) => without(current, sentIds));
    setSuccess(true);
    return true;
  }

  useImperativeHandle(saveRef, () => ({
    save: submit,
    isDirty: () => changed.length > 0,
  }));

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
    await refresh();
    setEdits((current) => without(current, [definition.id]));
    setServerErrors((current) => without(current, [definition.id]));
  }

  return (
    <form
      aria-label="Annotations"
      noValidate
      className="flex flex-col gap-3"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      {definitions.map((definition) => {
        const draft = draftOfRow(definition);
        const dirty = isDirty(draft, savedOf(definition));
        const saved = own.get(definition.name);
        // Only a row the user changed reports its own problems; an unchanged prefilled row does not.
        const error =
          (dirty ? draftError(draft, definition) : undefined) ?? serverErrors[definition.id];
        return (
          <DefinitionRow
            key={definition.id}
            definition={definition}
            draft={draft}
            own={saved}
            error={error}
            hint={dirty ? unsavedHint(draft, saved) : undefined}
            busy={busy}
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
      {hideSave ? null : (
        <div>
          <Button type="submit" size="sm" disabled={changed.length === 0 || !allValid || busy}>
            Save
          </Button>
        </div>
      )}
    </form>
  );
}

/** Why a changed row is not saved, in words for the row, or `undefined` when it is saved or reported. */
function unsavedHint(draft: Draft, own: Score | undefined): string | undefined {
  if (isSet(draft)) {
    return undefined;
  }
  if (own !== undefined) {
    return "Use Clear to remove your score.";
  }
  if (draft.reason.trim() !== "") {
    return "Set a value to save this row.";
  }
  return undefined;
}

function DefinitionRow({
  definition,
  draft,
  own,
  error,
  hint,
  busy,
  onChange,
  onClear,
}: {
  definition: ScoreDefinition;
  draft: Draft;
  own: Score | undefined;
  error: string | undefined;
  hint: string | undefined;
  busy: boolean;
  onChange: (draft: Draft) => void;
  onClear: () => void;
}) {
  const id = useId();
  const descriptionId = `${id}-description`;
  const rangeId = `${id}-range`;
  const errorId = `${id}-error`;
  const invalid = error !== undefined;
  const { name } = definition;
  const range =
    definition.data_type === "numeric"
      ? rangeText(definition.min_value, definition.max_value)
      : undefined;
  const controlDescribedBy = joinIds([
    definition.description ? descriptionId : undefined,
    range ? rangeId : undefined,
    invalid ? errorId : undefined,
  ]);

  return (
    <fieldset className="flex flex-col gap-1">
      <legend className="text-caption font-medium">{name}</legend>
      {definition.description ? (
        <p id={descriptionId} className="text-caption text-muted-foreground">
          {definition.description}
        </p>
      ) : null}
      {renderControl(definition, draft, {
        id,
        busy,
        invalid,
        describedBy: controlDescribedBy,
        range,
        rangeId,
        onChange,
      })}
      <div className="flex items-center gap-2">
        <Input
          aria-label={`${name} reason`}
          placeholder="Reason (optional)"
          value={draft.reason}
          aria-disabled={busy}
          aria-describedby={invalid ? errorId : undefined}
          className="aria-[disabled=true]:opacity-50"
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
            disabled={busy}
            onClick={onClear}
          >
            Clear
          </Button>
        ) : null}
      </div>
      {hint ? <p className="text-caption text-muted-foreground">{hint}</p> : null}
      {invalid ? (
        <p id={errorId} className="text-caption text-destructive">
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
    busy: boolean;
    invalid: boolean;
    describedBy: string | undefined;
    range: string | undefined;
    rangeId: string;
    onChange: (draft: Draft) => void;
  },
) {
  const { id, busy, invalid, describedBy, range, rangeId, onChange } = control;
  const { name } = definition;
  // Fields stay focusable while busy (aria-disabled, not disabled), so a save never drops focus.
  // Their changes are ignored in AnnotationRows.change.
  const fieldClass = "aria-[disabled=true]:opacity-50";
  switch (definition.data_type) {
    case "numeric":
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
            aria-disabled={busy}
            className={fieldClass}
            onChange={(event) => {
              onChange({ ...draft, value: event.target.value });
            }}
          />
          {range ? (
            <span id={rangeId} className="text-caption text-muted-foreground">
              {range}
            </span>
          ) : null}
        </div>
      );
    case "boolean":
      return (
        <RadioGroup
          aria-label={name}
          value={draft.value}
          aria-disabled={busy}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onValueChange={(value) => {
            onChange({ ...draft, value });
          }}
          className="flex gap-3 aria-[disabled=true]:opacity-50"
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
          aria-disabled={busy}
          aria-invalid={invalid}
          aria-describedby={describedBy}
          onValueChange={(value) => {
            onChange({ ...draft, value });
          }}
          className="flex flex-wrap gap-3 aria-[disabled=true]:opacity-50"
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
          aria-disabled={busy}
          className={fieldClass}
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

function joinIds(ids: (string | undefined)[]): string | undefined {
  const joined = ids.filter((id) => id !== undefined).join(" ");
  return joined === "" ? undefined : joined;
}

function without<T>(record: Record<string, T>, keys: readonly string[]): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([name]) => !keys.includes(name)));
}
