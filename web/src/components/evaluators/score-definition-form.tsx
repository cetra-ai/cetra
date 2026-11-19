import { useQueryClient } from "@tanstack/react-query";
import { Plus, X } from "lucide-react";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { scoreDefinitionsKey } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

export const SCORE_CANNOT_MANAGE = "Only members can manage score definitions";

export const SCORE_DATA_TYPE_LABELS: Record<Schema<"ScoreDataType">, string> = {
  numeric: "Numeric",
  boolean: "Boolean",
  categorical: "Categorical",
  text: "Text",
};

const DATA_TYPES: Schema<"ScoreDataType">[] = ["numeric", "boolean", "categorical", "text"];

const DIRECTION_LABELS: Record<Schema<"ScoreDirection">, string> = {
  higher: "Higher is better",
  lower: "Lower is better",
  none: "No direction",
};

const DIRECTIONS: Schema<"ScoreDirection">[] = ["higher", "lower", "none"];

/** Field errors the form renders under their field; any other one is shown above the button. */
const PLACED_FIELDS = new Set([
  "name",
  "categories",
  "min_value",
  "max_value",
  "direction",
  "description",
]);

const CLEAR_RANGE_ERROR = "A range cannot be cleared. Archive this definition and add a new one.";

function useCreateDefinition() {
  return $api.useMutation("post", "/api/v1/projects/{project}/score-definitions");
}

function usePatchDefinition() {
  return $api.useMutation("patch", "/api/v1/projects/{project}/score-definitions/{id}");
}

interface CategoryRow {
  id: number;
  label: string;
  value: string;
}

let categoryRowId = 0;

function newCategoryRow(label: string, value: string): CategoryRow {
  categoryRowId += 1;
  return { id: categoryRowId, label, value };
}

function optionalNumber(text: string): number | undefined {
  return text.trim() === "" ? undefined : Number(text);
}

/**
 * The create or edit form of a score definition, shown in the settings panel. Errors show once a
 * field was touched (or after a server refusal); Save waits for a valid, changed form.
 */
export function ScoreDefinitionForm({
  org,
  project,
  definition,
  canManage,
  onDone,
}: {
  org: string;
  project: string;
  /** Absent: create. Present: edit this definition. */
  definition: Schema<"ScoreDefinition"> | undefined;
  canManage: boolean;
  /** Called after a save. The panel closes. */
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const create = useCreateDefinition();
  const patch = usePatchDefinition();
  const pending = definition === undefined ? create.isPending : patch.isPending;
  const locked = pending || !canManage;
  const [name, setName] = useState(definition?.name ?? "");
  const [dataType, setDataType] = useState<Schema<"ScoreDataType">>(
    definition?.data_type ?? "numeric",
  );
  const [minValue, setMinValue] = useState(
    definition?.min_value !== undefined ? String(definition.min_value) : "",
  );
  const [maxValue, setMaxValue] = useState(
    definition?.max_value !== undefined ? String(definition.max_value) : "",
  );
  const [rows, setRows] = useState<CategoryRow[]>(() =>
    (definition?.categories ?? []).map((category) =>
      newCategoryRow(category.label, category.value === undefined ? "" : String(category.value)),
    ),
  );
  const [description, setDescription] = useState(definition?.description ?? "");
  const [direction, setDirection] = useState<Schema<"ScoreDirection">>(
    definition?.direction ?? "none",
  );
  const [touched, setTouched] = useState<ReadonlySet<string>>(() => new Set());
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();

  function touch(field: string) {
    setTouched((current) => (current.has(field) ? current : new Set(current).add(field)));
  }

  function categoryList(): Schema<"ScoreDefinitionCategory">[] {
    return rows
      .filter((row) => row.label.trim() !== "" || row.value.trim() !== "")
      .map((row) =>
        row.value.trim() === ""
          ? { label: row.label.trim() }
          : { label: row.label.trim(), value: Number(row.value) },
      );
  }

  const trimmedName = name.trim();
  const categories = dataType === "categorical" ? categoryList() : undefined;

  // A direction needs a range (numeric) or a value on every category (categorical), SD-3.
  const canChooseDirection =
    dataType === "numeric"
      ? optionalNumber(minValue) !== undefined && optionalNumber(maxValue) !== undefined
      : dataType === "categorical"
        ? categories !== undefined &&
          categories.length > 0 &&
          categories.every((category) => category.value !== undefined)
        : true;
  const directionHint = canChooseDirection
    ? undefined
    : dataType === "numeric"
      ? "Set a range to choose a direction."
      : "Give every category a value to choose a direction.";

  const clientErrors: Record<string, string> = {};
  if (trimmedName.length === 0) {
    clientErrors.name = "Enter a name.";
  }
  if (categories !== undefined) {
    if (categories.length === 0) {
      clientErrors.categories = "Add at least one category.";
    } else if (rows.some((row) => row.label.trim() === "" && row.value.trim() !== "")) {
      clientErrors.categories = "Enter a label for each category.";
    } else if (
      categories.some(
        (category) => category.value !== undefined && !Number.isFinite(category.value),
      )
    ) {
      clientErrors.categories = "Category values must be numbers.";
    }
  }
  if (dataType === "numeric" && definition !== undefined) {
    if (minValue.trim() === "" && definition.min_value !== undefined) {
      clientErrors.min_value = CLEAR_RANGE_ERROR;
    }
    if (maxValue.trim() === "" && definition.max_value !== undefined) {
      clientErrors.max_value = CLEAR_RANGE_ERROR;
    }
  }
  const invalid = Object.keys(clientErrors).length > 0;

  function errorOf(field: string): string | undefined {
    return serverErrors[field] ?? (touched.has(field) ? clientErrors[field] : undefined);
  }

  /** The changed fields of an edit. Empty when nothing changed. */
  function patchBody(current: Schema<"ScoreDefinition">): Schema<"UpdateScoreDefinitionRequest"> {
    const body: Schema<"UpdateScoreDefinitionRequest"> = {};
    if (trimmedName !== current.name) {
      body.name = trimmedName;
    }
    if (
      categories !== undefined &&
      JSON.stringify(categories) !== JSON.stringify(current.categories)
    ) {
      body.categories = categories;
    }
    if (dataType === "numeric") {
      const min = optionalNumber(minValue);
      const max = optionalNumber(maxValue);
      if (min !== undefined && min !== current.min_value) {
        body.min_value = min;
      }
      if (max !== undefined && max !== current.max_value) {
        body.max_value = max;
      }
    }
    if (dataType !== "text" && direction !== current.direction) {
      body.direction = direction;
    }
    if (description.trim() !== (current.description ?? "")) {
      body.description = description.trim();
    }
    return body;
  }

  const dirty = definition
    ? Object.keys(patchBody(definition)).length > 0
    : name !== "" ||
      dataType !== "numeric" ||
      minValue !== "" ||
      maxValue !== "" ||
      rows.length > 0 ||
      description.trim() !== "" ||
      direction !== "none";

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    setServerErrors({});
    if (invalid) {
      setTouched(new Set(Object.keys(clientErrors)));
      return;
    }
    if (definition === undefined) {
      const body: Schema<"CreateScoreDefinitionRequest"> = {
        name: trimmedName,
        data_type: dataType,
      };
      if (categories !== undefined) {
        body.categories = categories;
      }
      if (dataType === "numeric") {
        const min = optionalNumber(minValue);
        const max = optionalNumber(maxValue);
        if (min !== undefined) {
          body.min_value = min;
        }
        if (max !== undefined) {
          body.max_value = max;
        }
      }
      if (dataType !== "text" && direction !== "none") {
        body.direction = direction;
      }
      if (description.trim() !== "") {
        body.description = description.trim();
      }
      create.mutate(
        { params: { path: { project: projectRef(org, project) } }, body },
        { onSuccess: done, onError },
      );
      return;
    }
    const body = patchBody(definition);
    if (Object.keys(body).length === 0) {
      onDone();
      return;
    }
    patch.mutate(
      { params: { path: { project: projectRef(org, project), id: definition.id } }, body },
      { onSuccess: done, onError },
    );
  }

  function done() {
    void queryClient.invalidateQueries({ queryKey: scoreDefinitionsKey(org, project) });
    onDone();
  }

  function onError(caught: unknown) {
    const fields = fieldErrors(caught);
    const placed: Record<string, string> = {};
    const unplaced: string[] = [];
    for (const [field, message] of Object.entries(fields)) {
      if (PLACED_FIELDS.has(field)) {
        placed[field] = message;
      } else {
        unplaced.push(message);
      }
    }
    setServerErrors(placed);
    if (Object.keys(fields).length > 0) {
      setError(unplaced.length > 0 ? unplaced.join(" ") : undefined);
    } else {
      setError(problemMessage(caught, "Could not save the definition."));
    }
  }

  return (
    <>
      <form
        onSubmit={onSubmit}
        aria-label={definition ? "Edit score definition" : "Add score definition"}
        className="flex flex-col gap-4"
        noValidate
      >
        <TextField
          id="score-name"
          label="Name"
          value={name}
          onChange={(value) => {
            setName(value);
            touch("name");
          }}
          error={errorOf("name")}
          disabled={locked}
          autoComplete="off"
          maxLength={255}
        />
        {definition === undefined ? (
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="score-type">Type</Label>
            <Select
              value={dataType}
              disabled={locked}
              onValueChange={(value) => {
                const next = DATA_TYPES.find((type) => type === value);
                if (next) {
                  setDataType(next);
                }
              }}
            >
              <SelectTrigger id="score-type" className="w-fit">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {DATA_TYPES.map((type) => (
                  <SelectItem key={type} value={type}>
                    {SCORE_DATA_TYPE_LABELS[type]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        ) : (
          <TextField
            id="score-type"
            label="Type"
            value={SCORE_DATA_TYPE_LABELS[dataType]}
            onChange={() => {}}
            readOnly
            description="The type cannot change after creation."
          />
        )}
        {dataType === "numeric" ? (
          <div className="grid grid-cols-2 gap-3">
            <TextField
              id="score-min"
              label="Minimum"
              type="number"
              value={minValue}
              disabled={locked}
              onChange={(value) => {
                setMinValue(value);
                touch("min_value");
              }}
              error={errorOf("min_value")}
            />
            <TextField
              id="score-max"
              label="Maximum"
              type="number"
              value={maxValue}
              disabled={locked}
              onChange={(value) => {
                setMaxValue(value);
                touch("max_value");
              }}
              error={errorOf("max_value")}
            />
          </div>
        ) : null}
        {dataType === "categorical" ? (
          <div className="flex flex-col gap-2">
            <span className="text-body font-medium">Categories</span>
            {rows.map((row, index) => (
              <div key={row.id} className="flex items-center gap-2">
                <Input
                  aria-label={`Category label ${index + 1}`}
                  placeholder="Label"
                  value={row.label}
                  disabled={locked}
                  onChange={(event) => {
                    const label = event.target.value;
                    touch("categories");
                    setRows((current) =>
                      current.map((item) => (item.id === row.id ? { ...item, label } : item)),
                    );
                  }}
                />
                <Input
                  aria-label={`Category value ${index + 1}`}
                  placeholder="Value (optional)"
                  type="number"
                  value={row.value}
                  disabled={locked}
                  onChange={(event) => {
                    const value = event.target.value;
                    touch("categories");
                    setRows((current) =>
                      current.map((item) => (item.id === row.id ? { ...item, value } : item)),
                    );
                  }}
                />
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Remove category ${index + 1}`}
                  disabled={locked}
                  onClick={() => {
                    touch("categories");
                    setRows((current) => current.filter((item) => item.id !== row.id));
                  }}
                >
                  <X />
                </Button>
              </div>
            ))}
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="w-fit"
              disabled={locked}
              onClick={() => {
                touch("categories");
                setRows((current) => [...current, newCategoryRow("", "")]);
              }}
            >
              <Plus />
              Add category
            </Button>
            {errorOf("categories") ? (
              <p className="text-caption text-destructive">{errorOf("categories")}</p>
            ) : null}
          </div>
        ) : null}
        {dataType !== "text" ? (
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="score-direction">Direction</Label>
            <Select
              value={direction}
              disabled={locked}
              onValueChange={(value) => {
                const next = DIRECTIONS.find((item) => item === value);
                if (next) {
                  setDirection(next);
                }
              }}
            >
              <SelectTrigger id="score-direction" className="w-fit">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {DIRECTIONS.map((item) => (
                  <SelectItem
                    key={item}
                    value={item}
                    disabled={item !== "none" && !canChooseDirection}
                  >
                    {DIRECTION_LABELS[item]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {directionHint ? (
              <p className="text-caption text-muted-foreground">{directionHint}</p>
            ) : null}
            {errorOf("direction") ? (
              <p className="text-caption text-destructive">{errorOf("direction")}</p>
            ) : null}
          </div>
        ) : null}
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="score-description">Description</Label>
          <Textarea
            id="score-description"
            value={description}
            disabled={locked}
            onChange={(event) => {
              setDescription(event.target.value);
              touch("description");
            }}
            aria-invalid={errorOf("description") ? true : undefined}
            rows={3}
          />
          {errorOf("description") ? (
            <p className="text-caption text-destructive">{errorOf("description")}</p>
          ) : null}
        </div>
        <FormError message={error} />
        <div className="flex items-center justify-end gap-3">
          <div className="mr-auto">
            {dirty ? (
              <span className="text-caption text-muted-foreground">Unsaved changes</span>
            ) : null}
          </div>
          <DisabledReason reason={canManage ? undefined : SCORE_CANNOT_MANAGE}>
            <SubmitButton pending={pending} disabled={!dirty || invalid || !canManage}>
              {definition === undefined ? "Add definition" : "Save"}
            </SubmitButton>
          </DisabledReason>
        </div>
      </form>
      <UnsavedChangesGuard dirty={dirty && !pending} />
    </>
  );
}
