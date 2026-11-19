import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { orgPricesKey, PROJECT_PRICES_PREFIX, PROJECT_RESOLVE_PREFIX } from "@/api/queries-prices";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { PRICE_UNIT, TiersEditor } from "@/components/prices/tiers-editor";
import { SettingsPanel } from "@/components/settings/settings-list";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  blockedByRules,
  checkPriceForm,
  type PriceForm as PriceFormState,
  type PriceInput,
  priceBody,
  priceForm,
} from "@/lib/prices";

export type PriceScope =
  | { kind: "org"; org: string }
  | { kind: "project"; org: string; project: string };

type PriceRow = Schema<"ModelPrice">;

const FORM_ID = "price-form";

/** The server's names of the five price fields. A change to any of them can change their errors. */
const PRICE_NAMES = [
  "price_input",
  "price_output",
  "price_cache_read",
  "price_cache_write",
  "price_reasoning",
] as const;

/** Fields the form renders; only server errors for these are placed under the field. */
const PLACED_FIELDS = new Set([
  "model_name",
  "match_pattern",
  "provider",
  "valid_from",
  ...PRICE_NAMES,
]);

/** The form fields that hold typed text. */
type TextKey =
  | "modelName"
  | "pattern"
  | "provider"
  | "validFrom"
  | "input"
  | "output"
  | "cacheRead"
  | "cacheWrite"
  | "reasoning";

/** Every list of prices of the org, and of the project, is stale after a change. */
export function refreshPrices(queryClient: QueryClient, org: string) {
  void queryClient.invalidateQueries({ queryKey: orgPricesKey(org) });
  void queryClient.invalidateQueries({ queryKey: [...PROJECT_PRICES_PREFIX] });
  void queryClient.invalidateQueries({ queryKey: [...PROJECT_RESOLVE_PREFIX] });
}

/**
 * The panel that adds, edits or deletes one price. Without `row` it adds. Fields are checked as
 * the user types, but an error shows only once its field was changed or a save was tried.
 * A save or delete calls `onDone`; closing calls `onClose`.
 */
export function PriceFormPanel({
  scope,
  row,
  readOnlyReason,
  onClose,
  onDone,
}: {
  scope: PriceScope;
  /** The row to edit; absent to add. */
  row?: PriceRow;
  /** Set for a user who cannot change prices: the fields are read-only and this says why. */
  readOnlyReason?: string;
  onClose: () => void;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const orgCreate = $api.useMutation("post", "/api/v1/orgs/{org}/models");
  const orgPatch = $api.useMutation("patch", "/api/v1/orgs/{org}/models/{id}");
  const orgRemove = $api.useMutation("delete", "/api/v1/orgs/{org}/models/{id}");
  const projectCreate = $api.useMutation("post", "/api/v1/projects/{project}/models");
  const projectPatch = $api.useMutation("patch", "/api/v1/projects/{project}/models/{id}");
  const projectRemove = $api.useMutation("delete", "/api/v1/projects/{project}/models/{id}");

  const editing = row?.id !== undefined;
  const saving =
    scope.kind === "org"
      ? (editing ? orgPatch : orgCreate).isPending
      : (editing ? projectPatch : projectCreate).isPending;
  const removing = scope.kind === "org" ? orgRemove : projectRemove;
  const pending = saving || removing.isPending;
  const readOnly = readOnlyReason !== undefined;
  const locked = readOnly || pending;

  const [initial] = useState(() => priceForm(row));
  const [form, setForm] = useState<PriceFormState>(initial);
  const [touched, setTouched] = useState<ReadonlySet<string>>(() => new Set());
  const [submitted, setSubmitted] = useState(false);
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({});
  const [formError, setFormError] = useState<string | undefined>();
  const [confirmOpen, setConfirmOpen] = useState(false);

  const clientErrors = checkPriceForm(form);
  const invalid = Object.keys(clientErrors).length > 0;
  const dirty = !readOnly && JSON.stringify(priceBody(form)) !== JSON.stringify(priceBody(initial));
  const shownErrors = Object.fromEntries(
    Object.entries(clientErrors).filter(([name]) => submitted || touched.has(name)),
  );
  const errors: Record<string, string> = { ...shownErrors, ...serverErrors };

  function update<K extends keyof PriceFormState>(key: K, value: PriceFormState[K]) {
    setForm((current) => ({ ...current, [key]: value }));
    setServerErrors({});
  }

  function touch(names: readonly string[]) {
    setTouched((current) => new Set([...current, ...names]));
  }

  function text(key: TextKey, names: readonly string[]) {
    return (value: string) => {
      update(key, value);
      touch(names);
    };
  }

  function done() {
    refreshPrices(queryClient, scope.org);
    onDone();
  }

  function onError(caught: unknown) {
    // A 409 on a budgeted rule is explained in words; the form has no field for it.
    const blocked = blockedByRules(caught);
    if (blocked !== undefined) {
      setServerErrors({});
      setFormError(blocked);
      return;
    }
    const placed: Record<string, string> = {};
    const unplaced: string[] = [];
    for (const [name, message] of Object.entries(fieldErrors(caught))) {
      if (PLACED_FIELDS.has(name) || name.startsWith("tiers[")) {
        placed[name] = message;
      } else {
        // A field the form does not render: the user still has to see it.
        unplaced.push(message);
      }
    }
    setServerErrors(placed);
    if (unplaced.length > 0) {
      setFormError(unplaced.join(" "));
    } else if (Object.keys(placed).length === 0) {
      setFormError(problemMessage(caught, "Could not save the price."));
    }
  }

  function save(body: PriceInput) {
    const handlers = { onSuccess: done, onError };
    if (scope.kind === "org") {
      if (row?.id === undefined) {
        orgCreate.mutate({ params: { path: { org: scope.org } }, body }, handlers);
      } else {
        orgPatch.mutate({ params: { path: { org: scope.org, id: row.id } }, body }, handlers);
      }
      return;
    }
    const project = projectRef(scope.org, scope.project);
    if (row?.id === undefined) {
      projectCreate.mutate({ params: { path: { project } }, body }, handlers);
    } else {
      projectPatch.mutate({ params: { path: { project, id: row.id } }, body }, handlers);
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setSubmitted(true);
    setFormError(undefined);
    setServerErrors({});
    if (invalid || !dirty || pending) {
      return;
    }
    save(priceBody(form));
  }

  function remove() {
    if (row?.id === undefined) {
      return;
    }
    if (scope.kind === "org") {
      orgRemove.mutate({ params: { path: { org: scope.org, id: row.id } } }, { onSuccess: done });
      return;
    }
    projectRemove.mutate(
      { params: { path: { project: projectRef(scope.org, scope.project), id: row.id } } },
      { onSuccess: done },
    );
  }

  const removeError = removing.error
    ? (blockedByRules(removing.error) ??
      problemMessage(removing.error, "Could not delete the price."))
    : undefined;

  return (
    <>
      <SettingsPanel
        title={editing ? "Edit model price" : "Add model price"}
        subtitle={row?.model_name}
        onClose={onClose}
        footer={
          readOnly ? undefined : (
            <>
              {row?.id === undefined ? null : (
                <Button
                  type="button"
                  variant="ghost"
                  className="mr-auto text-destructive"
                  disabled={pending}
                  onClick={() => {
                    setConfirmOpen(true);
                  }}
                >
                  Delete
                </Button>
              )}
              <SubmitButton form={FORM_ID} pending={saving} disabled={!dirty || invalid}>
                {editing ? "Save" : "Add price"}
              </SubmitButton>
            </>
          )
        }
      >
        {readOnlyReason === undefined ? null : (
          <p className="text-body text-muted-foreground">{readOnlyReason}</p>
        )}
        <form
          id={FORM_ID}
          aria-label={editing ? "Edit model price" : "Add model price"}
          noValidate
          className="flex flex-col gap-5"
          onSubmit={onSubmit}
        >
          <TextField
            id="price-model-name"
            label="Model name"
            value={form.modelName}
            error={errors.model_name}
            disabled={locked}
            onChange={text("modelName", ["model_name"])}
          />
          <fieldset className="flex flex-col gap-2">
            <legend className="text-body leading-none font-medium">Match</legend>
            <RadioGroup
              value={form.matchKind}
              disabled={locked}
              className="flex flex-row gap-6"
              onValueChange={(value) => {
                update("matchKind", value === "regex" ? "regex" : "exact");
                touch(["match_pattern"]);
              }}
            >
              <div className="flex items-center gap-2">
                <RadioGroupItem value="exact" id="price-match-exact" />
                <Label htmlFor="price-match-exact">Exact name</Label>
              </div>
              <div className="flex items-center gap-2">
                <RadioGroupItem value="regex" id="price-match-regex" />
                <Label htmlFor="price-match-regex">Regular expression</Label>
              </div>
            </RadioGroup>
          </fieldset>
          {form.matchKind === "regex" ? (
            <TextField
              id="price-pattern"
              label="Pattern"
              value={form.pattern}
              error={errors.match_pattern}
              disabled={locked}
              onChange={text("pattern", ["match_pattern"])}
              description="Anchored and case-insensitive, e.g. llama3.*. Without a provider, the pattern overrides the built-in price of every model it matches, for every provider."
            />
          ) : null}
          <TextField
            id="price-provider"
            label="Provider"
            value={form.provider}
            error={errors.provider}
            disabled={locked}
            onChange={text("provider", [])}
            description="Leave empty to match any provider. Local models served through an OpenAI-compatible endpoint use openai."
          />
          <TextField
            id="price-valid-from"
            label="Valid from"
            type="date"
            value={form.validFrom}
            error={errors.valid_from}
            disabled={locked}
            onChange={text("validFrom", [])}
            description="Leave empty to apply to every span."
          />
          <div className="grid grid-cols-2 gap-4">
            <TextField
              id="price-input"
              label="Input"
              type="number"
              unit={PRICE_UNIT}
              min={0}
              value={form.input}
              error={errors.price_input}
              disabled={locked}
              onChange={text("input", PRICE_NAMES)}
            />
            <TextField
              id="price-output"
              label="Output"
              type="number"
              unit={PRICE_UNIT}
              min={0}
              value={form.output}
              error={errors.price_output}
              disabled={locked}
              onChange={text("output", PRICE_NAMES)}
            />
            <TextField
              id="price-cache-read"
              label="Cache read"
              type="number"
              unit={PRICE_UNIT}
              min={0}
              value={form.cacheRead}
              error={errors.price_cache_read}
              disabled={locked}
              onChange={text("cacheRead", PRICE_NAMES)}
            />
            <TextField
              id="price-cache-write"
              label="Cache write"
              type="number"
              unit={PRICE_UNIT}
              min={0}
              value={form.cacheWrite}
              error={errors.price_cache_write}
              disabled={locked}
              onChange={text("cacheWrite", PRICE_NAMES)}
            />
            <TextField
              id="price-reasoning"
              label="Reasoning"
              type="number"
              unit={PRICE_UNIT}
              min={0}
              value={form.reasoning}
              error={errors.price_reasoning}
              disabled={locked}
              onChange={text("reasoning", PRICE_NAMES)}
              description="Empty uses the output price."
            />
          </div>
          <p className="text-caption text-muted-foreground">
            A judge model with a budget needs both an input and an output price.
          </p>
          <div className="flex flex-col gap-3">
            <h3 className="text-body leading-none font-medium">Long-context tiers</h3>
            <TiersEditor
              tiers={form.tiers}
              errors={errors}
              disabled={locked}
              onChange={(tiers) => {
                update("tiers", tiers);
              }}
              onTouch={touch}
            />
          </div>
          <FormError message={formError} />
        </form>
      </SettingsPanel>
      <UnsavedChangesGuard dirty={dirty && !pending} />
      {row?.id === undefined ? null : (
        <TypeToConfirmDialog
          open={confirmOpen}
          onOpenChange={setConfirmOpen}
          title="Delete this price?"
          description="Spans received after this use the next matching price. Stored costs do not change."
          expected={row.model_name}
          inputLabel={`Type "${row.model_name}" to confirm`}
          confirmLabel="Delete"
          pending={removing.isPending}
          error={removeError}
          onConfirm={remove}
        />
      )}
    </>
  );
}
