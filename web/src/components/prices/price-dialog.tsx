import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { orgPricesKey, PROJECT_PRICES_PREFIX, PROJECT_RESOLVE_PREFIX } from "@/api/queries-prices";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { TiersEditor } from "@/components/prices/tiers-editor";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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

/** Fields the form renders; only server errors for these are placed under the field. */
const PLACED_FIELDS = new Set([
  "model_name",
  "match_pattern",
  "provider",
  "valid_from",
  "price_input",
  "price_output",
  "price_cache_read",
  "price_cache_write",
  "price_reasoning",
]);

interface SaveHandlers {
  onSuccess: () => void;
  onError: (error: unknown) => void;
}

export function PriceDialog({
  scope,
  open,
  onOpenChange,
  row,
}: {
  scope: PriceScope;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The row to edit; absent to create. */
  row?: Schema<"ModelPrice">;
}) {
  const orgCreate = $api.useMutation("post", "/api/v1/orgs/{org}/models");
  const orgPatch = $api.useMutation("patch", "/api/v1/orgs/{org}/models/{id}");
  const projectCreate = $api.useMutation("post", "/api/v1/projects/{project}/models");
  const projectPatch = $api.useMutation("patch", "/api/v1/projects/{project}/models/{id}");
  const editing = row?.id !== undefined;
  const pending =
    scope.kind === "org"
      ? (editing ? orgPatch : orgCreate).isPending
      : (editing ? projectPatch : projectCreate).isPending;

  // Unmounting the form on close drops its state with it, so each open starts from the row.
  function close() {
    orgCreate.reset();
    orgPatch.reset();
    projectCreate.reset();
    projectPatch.reset();
    onOpenChange(false);
  }

  function save(body: PriceInput, handlers: SaveHandlers) {
    const id = row?.id;
    if (scope.kind === "org") {
      if (id === undefined) {
        orgCreate.mutate({ params: { path: { org: scope.org } }, body }, handlers);
      } else {
        orgPatch.mutate({ params: { path: { org: scope.org, id } }, body }, handlers);
      }
      return;
    }
    const project = projectRef(scope.org, scope.project);
    if (id === undefined) {
      projectCreate.mutate({ params: { path: { project } }, body }, handlers);
    } else {
      projectPatch.mutate({ params: { path: { project, id } }, body }, handlers);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // A request in flight keeps the dialog open, so its result is not lost.
        if (next) {
          onOpenChange(true);
          return;
        }
        if (!pending) {
          close();
        }
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto sm:max-w-2xl"
        onEscapeKeyDown={(event) => {
          if (pending) {
            event.preventDefault();
          }
        }}
        onPointerDownOutside={(event) => {
          if (pending) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{row ? "Edit model price" : "Add model price"}</DialogTitle>
          <DialogDescription>
            Prices are USD per 1M tokens. New prices apply to spans received from now on; stored
            costs are not recomputed.
          </DialogDescription>
        </DialogHeader>
        {open ? (
          <PriceEditor scope={scope} row={row} pending={pending} save={save} onDone={close} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

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

function PriceEditor({
  scope,
  row,
  pending,
  save,
  onDone,
}: {
  scope: PriceScope;
  row: Schema<"ModelPrice"> | undefined;
  pending: boolean;
  save: (body: PriceInput, handlers: SaveHandlers) => void;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const [form, setForm] = useState<PriceFormState>(() => priceForm(row));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | undefined>();
  const update = <K extends keyof PriceFormState>(key: K, value: PriceFormState[K]) => {
    setForm((current) => ({ ...current, [key]: value }));
  };
  const field = (key: TextKey) => (value: string) => {
    update(key, value);
  };

  function done() {
    void queryClient.invalidateQueries({ queryKey: orgPricesKey(scope.org) });
    void queryClient.invalidateQueries({ queryKey: [...PROJECT_PRICES_PREFIX] });
    void queryClient.invalidateQueries({ queryKey: [...PROJECT_RESOLVE_PREFIX] });
    onDone();
  }

  function onError(caught: unknown) {
    // A 409 on a budgeted rule is explained in words; the form has no field for it.
    const blocked = blockedByRules(caught);
    if (blocked !== undefined) {
      setErrors({});
      setError(blocked);
      return;
    }
    const next: Record<string, string> = {};
    const unplaced: string[] = [];
    for (const [name, message] of Object.entries(fieldErrors(caught))) {
      if (PLACED_FIELDS.has(name) || name.startsWith("tiers[")) {
        next[name] = message;
      } else {
        // A field the form does not render: the user still has to see it.
        unplaced.push(message);
      }
    }
    setErrors(next);
    if (unplaced.length > 0) {
      setError(unplaced.join(" "));
    } else if (Object.keys(next).length === 0) {
      setError(problemMessage(caught, "Could not save the price."));
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setError(undefined);
    const nextErrors = checkPriceForm(form);
    setErrors(nextErrors);
    if (Object.keys(nextErrors).length > 0) {
      return;
    }
    save(priceBody(form), { onSuccess: done, onError });
  }

  return (
    <form noValidate className="flex flex-col gap-5" onSubmit={onSubmit}>
      <TextField
        id="price-model-name"
        label="Model name"
        value={form.modelName}
        error={errors.model_name}
        disabled={pending}
        onChange={field("modelName")}
      />
      <fieldset className="flex flex-col gap-2">
        <legend className="text-sm leading-none font-medium">Match</legend>
        <RadioGroup
          value={form.matchKind}
          disabled={pending}
          className="flex flex-row gap-6"
          onValueChange={(value) => {
            update("matchKind", value === "regex" ? "regex" : "exact");
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
          disabled={pending}
          onChange={field("pattern")}
          description="Anchored and case-insensitive, e.g. llama3.*. Without a provider, the pattern overrides the built-in price of every model it matches, for every provider."
        />
      ) : null}
      <TextField
        id="price-provider"
        label="Provider"
        value={form.provider}
        error={errors.provider}
        disabled={pending}
        onChange={field("provider")}
        description="Leave empty to match any provider. Local models served through an OpenAI-compatible endpoint use openai."
      />
      <TextField
        id="price-valid-from"
        label="Valid from"
        type="date"
        value={form.validFrom}
        error={errors.valid_from}
        disabled={pending}
        onChange={field("validFrom")}
        description="Leave empty to apply to every span."
      />
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-5">
        <TextField
          id="price-input"
          label="Input"
          type="number"
          value={form.input}
          error={errors.price_input}
          disabled={pending}
          onChange={field("input")}
        />
        <TextField
          id="price-output"
          label="Output"
          type="number"
          value={form.output}
          error={errors.price_output}
          disabled={pending}
          onChange={field("output")}
        />
        <TextField
          id="price-cache-read"
          label="Cache read"
          type="number"
          value={form.cacheRead}
          error={errors.price_cache_read}
          disabled={pending}
          onChange={field("cacheRead")}
        />
        <TextField
          id="price-cache-write"
          label="Cache write"
          type="number"
          value={form.cacheWrite}
          error={errors.price_cache_write}
          disabled={pending}
          onChange={field("cacheWrite")}
        />
        <TextField
          id="price-reasoning"
          label="Reasoning"
          type="number"
          value={form.reasoning}
          error={errors.price_reasoning}
          disabled={pending}
          onChange={field("reasoning")}
          description="Empty uses the output price."
        />
      </div>
      <p className="text-xs text-muted-foreground">
        A judge model with a budget needs both an input and an output price.
      </p>
      <div className="flex flex-col gap-3">
        <h3 className="text-sm leading-none font-medium">Long-context tiers</h3>
        <TiersEditor
          tiers={form.tiers}
          errors={errors}
          disabled={pending}
          onChange={(tiers) => {
            update("tiers", tiers);
          }}
        />
      </div>
      <FormError message={error} />
      <DialogFooter>
        <Button type="button" variant="outline" disabled={pending} onClick={onDone}>
          Cancel
        </Button>
        <SubmitButton pending={pending}>{row ? "Save" : "Add price"}</SubmitButton>
      </DialogFooter>
    </form>
  );
}
