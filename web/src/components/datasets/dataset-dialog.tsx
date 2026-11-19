import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetKey, datasetsKey } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { useProject } from "@/components/shell/project-context";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField, prettyJson, sameJson } from "@/lib/datasets";

/** The API's detail for a name that is already taken in the project. */
const NAME_TAKEN_DETAIL = "a dataset with this name already exists";
const SCHEMA_LINES_SHOWN = 5;
const FALLBACK = "The dataset could not be saved";
/** Field errors the form renders under their own field; `items[...]` ones go in the batch block. */
const PLACED_FIELDS = ["name", "description", "input_schema", "expected_schema"];

type SchemaValue = Schema<"CreateDatasetRequest">["input_schema"];

/**
 * Parsed JSON is `unknown`, but the generated schema type is `Record<string, never>`
 * (the same cast as evaluator-form.tsx's config).
 */
function asSchema(value: unknown): SchemaValue {
  return value as SchemaValue;
}

function useCreateDataset() {
  return $api.useMutation("post", "/api/v1/projects/{project}/datasets");
}

function usePatchDataset() {
  return $api.useMutation("patch", "/api/v1/projects/{project}/datasets/{dataset}");
}

export function DatasetDialog({
  open,
  onOpenChange,
  dataset,
  onSaved,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Absent: create. Present: edit this dataset (J5.3 uses it in the settings tab). */
  dataset?: Schema<"Dataset">;
  /** Called with the saved dataset after a success. */
  onSaved?: (dataset: Schema<"Dataset">) => void;
}) {
  const create = useCreateDataset();
  const patch = usePatchDataset();
  const pending = dataset === undefined ? create.isPending : patch.isPending;
  // A form the user has typed in is not closed by Escape or a click outside (the X still closes it).
  const [dirty, setDirty] = useState(false);
  const keepOpen = pending || dirty;

  // Unmounting the form on close drops its state with it, so reopening starts clean.
  function close() {
    create.reset();
    patch.reset();
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !pending) {
          close();
        }
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onInteractOutside={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{dataset ? "Edit dataset" : "New dataset"}</DialogTitle>
          <DialogDescription>
            Schemas are checked on every write: an item must match them to be saved.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <DatasetForm
            dataset={dataset}
            create={create}
            patch={patch}
            onSaved={onSaved}
            onDirty={() => {
              setDirty(true);
            }}
            onDone={close}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function DatasetForm({
  dataset,
  create,
  patch,
  onSaved,
  onDirty,
  onDone,
}: {
  dataset: Schema<"Dataset"> | undefined;
  create: ReturnType<typeof useCreateDataset>;
  patch: ReturnType<typeof usePatchDataset>;
  onSaved: ((dataset: Schema<"Dataset">) => void) | undefined;
  onDirty: () => void;
  onDone: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const [name, setName] = useState(dataset?.name ?? "");
  const [description, setDescription] = useState(dataset?.description ?? "");
  const [inputText, setInputText] = useState(prettyJson(dataset?.input_schema));
  const [expectedText, setExpectedText] = useState(prettyJson(dataset?.expected_schema));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [batchErrors, setBatchErrors] = useState<string[]>([]);
  const [error, setError] = useState<string | undefined>();
  const pending = dataset === undefined ? create.isPending : patch.isPending;

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setErrors({});
    setBatchErrors([]);
    const trimmedName = name.trim();
    const input = parseJsonField(inputText, { required: false, object: true });
    const expected = parseJsonField(expectedText, { required: false, object: true });
    const nextErrors: Record<string, string> = {};
    if (trimmedName === "") {
      nextErrors.name = "Enter a name.";
    }
    if (!input.ok) {
      nextErrors.input_schema = input.message;
    }
    if (!expected.ok) {
      nextErrors.expected_schema = expected.message;
    }
    if (Object.keys(nextErrors).length > 0 || !input.ok || !expected.ok) {
      setErrors(nextErrors);
      return;
    }
    const trimmedDescription = description.trim();

    if (dataset === undefined) {
      const body: Schema<"CreateDatasetRequest"> = { name: trimmedName };
      if (trimmedDescription !== "") {
        body.description = trimmedDescription;
      }
      if (input.value !== undefined) {
        body.input_schema = asSchema(input.value);
      }
      if (expected.value !== undefined) {
        body.expected_schema = asSchema(expected.value);
      }
      create.mutate(
        { params: { path: { project: projectRef(org, project) } }, body },
        { onSuccess: saved, onError },
      );
      return;
    }

    // Only changed fields are sent. A description or schema that is now empty is sent as null (D-J-2).
    const body: Schema<"UpdateDatasetRequest"> = {};
    if (trimmedName !== dataset.name) {
      body.name = trimmedName;
    }
    if (trimmedDescription !== (dataset.description ?? "")) {
      body.description = trimmedDescription === "" ? null : trimmedDescription;
    }
    if (!sameJson(input.value, dataset.input_schema)) {
      body.input_schema = input.value === undefined ? null : asSchema(input.value);
    }
    if (!sameJson(expected.value, dataset.expected_schema)) {
      body.expected_schema = expected.value === undefined ? null : asSchema(expected.value);
    }
    if (Object.keys(body).length === 0) {
      onDone();
      return;
    }
    patch.mutate(
      { params: { path: { project: projectRef(org, project), dataset: dataset.id } }, body },
      { onSuccess: saved, onError },
    );
  }

  function saved(result: Schema<"Dataset">) {
    void queryClient.invalidateQueries({ queryKey: datasetsKey(org, project) });
    if (dataset !== undefined) {
      void queryClient.invalidateQueries({ queryKey: datasetKey(org, project, dataset.id) });
    }
    onSaved?.(result);
    onDone();
  }

  function onError(caught: unknown) {
    const problem = problemOf(caught);
    const placed: Record<string, string> = {};
    const batch: string[] = [];
    const other: string[] = [];
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      if (PLACED_FIELDS.includes(field)) {
        placed[field] = message;
      } else if (field.startsWith("items[")) {
        batch.push(message);
      } else {
        other.push(message);
      }
    }
    if (problem?.status === 409 && problem.detail === NAME_TAKEN_DETAIL) {
      placed.name = "A dataset with this name exists";
    }
    setErrors(placed);
    setBatchErrors(batch);
    if (other.length > 0) {
      setError(other.join(" "));
    } else if (Object.keys(placed).length === 0 && batch.length === 0) {
      setError(problemMessage(caught, FALLBACK));
    }
  }

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <TextField
        id="dataset-name"
        label="Name"
        value={name}
        onChange={(value) => {
          setName(value);
          onDirty();
        }}
        error={errors.name}
        autoComplete="off"
        maxLength={200}
      />
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dataset-description">Description</Label>
        <Textarea
          id="dataset-description"
          value={description}
          maxLength={2000}
          rows={2}
          aria-invalid={errors.description ? true : undefined}
          onChange={(event) => {
            setDescription(event.target.value);
            onDirty();
          }}
        />
        {errors.description ? (
          <p className="text-caption text-destructive">{errors.description}</p>
        ) : null}
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dataset-input-schema">Input schema</Label>
        <Textarea
          id="dataset-input-schema"
          className="font-mono text-caption"
          spellCheck={false}
          rows={5}
          value={inputText}
          aria-invalid={errors.input_schema ? true : undefined}
          aria-describedby="dataset-input-schema-hint"
          onChange={(event) => {
            setInputText(event.target.value);
            onDirty();
          }}
        />
        <p id="dataset-input-schema-hint" className="text-caption text-muted-foreground">
          A JSON Schema every item's input must match. Leave empty for none.
        </p>
        {errors.input_schema ? (
          <p className="text-caption text-destructive">{errors.input_schema}</p>
        ) : null}
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="dataset-expected-schema">Expected output schema</Label>
        <Textarea
          id="dataset-expected-schema"
          className="font-mono text-caption"
          spellCheck={false}
          rows={5}
          value={expectedText}
          aria-invalid={errors.expected_schema ? true : undefined}
          aria-describedby="dataset-expected-schema-hint"
          onChange={(event) => {
            setExpectedText(event.target.value);
            onDirty();
          }}
        />
        <p id="dataset-expected-schema-hint" className="text-caption text-muted-foreground">
          A JSON Schema every item's expected output must match. Leave empty for none.
        </p>
        {errors.expected_schema ? (
          <p className="text-caption text-destructive">{errors.expected_schema}</p>
        ) : null}
      </div>
      {batchErrors.length > 0 ? (
        <div role="alert" className="flex flex-col gap-0.5 text-body text-destructive">
          <p className="whitespace-pre-line">
            {batchErrors.slice(0, SCHEMA_LINES_SHOWN).join("\n")}
          </p>
          {batchErrors.length > SCHEMA_LINES_SHOWN ? (
            <p>and {batchErrors.length - SCHEMA_LINES_SHOWN} more</p>
          ) : null}
        </div>
      ) : null}
      <FormError message={error} />
      <DialogFooter>
        <SubmitButton pending={pending}>
          {dataset === undefined ? "Create dataset" : "Save"}
        </SubmitButton>
      </DialogFooter>
    </form>
  );
}
