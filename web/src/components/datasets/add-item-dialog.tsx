import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { useProject } from "@/components/shell/project-context";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField } from "@/lib/datasets";

const FALLBACK = "The item could not be added";
const PLACED_FIELDS = ["input", "expected_output", "metadata"] as const;

type ItemField = (typeof PLACED_FIELDS)[number];
type Metadata = Schema<"NewDatasetItem">["metadata"];

function useAddItem() {
  return $api.useMutation("post", "/api/v1/projects/{project}/datasets/{dataset}/items");
}

/** Adds one item to the latest version of a dataset. */
export function AddItemDialog({
  datasetId,
  open,
  onOpenChange,
}: {
  datasetId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const add = useAddItem();

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !add.isPending) {
          add.reset();
          onOpenChange(false);
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Add item</DialogTitle>
          <DialogDescription>Adds the item to a new version of the dataset.</DialogDescription>
        </DialogHeader>
        {open ? (
          <AddItemForm
            datasetId={datasetId}
            add={add}
            onDone={() => {
              add.reset();
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function AddItemForm({
  datasetId,
  add,
  onDone,
}: {
  datasetId: string;
  add: ReturnType<typeof useAddItem>;
  onDone: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const [inputText, setInputText] = useState("");
  const [expectedText, setExpectedText] = useState("");
  const [metadataText, setMetadataText] = useState("");
  const [errors, setErrors] = useState<Partial<Record<ItemField, string>>>({});
  const [formError, setFormError] = useState<string | undefined>();

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    const input = parseJsonField(inputText, { required: true });
    const expected = parseJsonField(expectedText, { required: false });
    const metadata = parseJsonField(metadataText, { required: false, object: true });
    const nextErrors: Partial<Record<ItemField, string>> = {};
    if (!input.ok) {
      nextErrors.input = input.message;
    }
    if (!expected.ok) {
      nextErrors.expected_output = expected.message;
    }
    if (!metadata.ok) {
      nextErrors.metadata = metadata.message;
    }
    setErrors(nextErrors);
    if (!input.ok || !expected.ok || !metadata.ok) {
      return;
    }

    const item: Schema<"NewDatasetItem"> = { input: input.value };
    if (expected.value !== undefined) {
      item.expected_output = expected.value;
    }
    if (metadata.value !== undefined) {
      item.metadata = metadata.value as Metadata;
    }
    add.mutate(
      {
        params: { path: { project: projectRef(org, project), dataset: datasetId } },
        body: { items: [item] },
      },
      {
        onSuccess: () => {
          for (const key of datasetWriteKeys(org, project, datasetId)) {
            void queryClient.invalidateQueries({ queryKey: key });
          }
          onDone();
        },
        onError: showErrors,
      },
    );
  }

  // The batch endpoint names a field of its one item as `items[0].<field>`.
  function showErrors(caught: unknown) {
    const placed: Partial<Record<ItemField, string>> = {};
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      const name = field.replace(/^items\[\d+\]\./, "");
      const known = PLACED_FIELDS.find((candidate) => candidate === name);
      if (known) {
        placed[known] = message;
      }
    }
    setErrors(placed);
    if (Object.keys(placed).length === 0) {
      setFormError(problemMessage(caught, FALLBACK));
    }
  }

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <JsonField label="Input" value={inputText} onChange={setInputText} error={errors.input} />
      <JsonField
        label="Expected output"
        value={expectedText}
        onChange={setExpectedText}
        error={errors.expected_output}
      />
      <JsonField
        label="Metadata"
        value={metadataText}
        onChange={setMetadataText}
        error={errors.metadata}
      />
      <FormError message={formError} />
      <DialogFooter>
        <SubmitButton pending={add.isPending}>Add item</SubmitButton>
      </DialogFooter>
    </form>
  );
}

function JsonField({
  label,
  value,
  onChange,
  error,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  error?: string;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <span className="text-sm font-medium">{label}</span>
      <Textarea
        aria-label={label}
        className="font-mono text-xs"
        spellCheck={false}
        rows={4}
        value={value}
        aria-invalid={error ? true : undefined}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      {error ? <p className="text-xs text-destructive">{error}</p> : null}
    </div>
  );
}
