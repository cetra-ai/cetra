import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { type ItemField, JsonField, splitItemErrors } from "@/components/datasets/item-fields";
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
import { parseJsonField } from "@/lib/datasets";

const FALLBACK = "The item could not be added";

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
  // A form the user has typed in is not closed by Escape or a click outside (the X still closes it).
  const [dirty, setDirty] = useState(false);
  const keepOpen = add.isPending || dirty;

  function close() {
    add.reset();
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !add.isPending) {
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
          <DialogTitle>Add item</DialogTitle>
          <DialogDescription>Adds the item to a new version of the dataset.</DialogDescription>
        </DialogHeader>
        {open ? (
          <AddItemForm
            datasetId={datasetId}
            add={add}
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

function AddItemForm({
  datasetId,
  add,
  onDirty,
  onDone,
}: {
  datasetId: string;
  add: ReturnType<typeof useAddItem>;
  onDirty: () => void;
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
    const { placed, other } = splitItemErrors(caught);
    setErrors(placed);
    if (other.length > 0 || Object.keys(placed).length === 0) {
      setFormError(other.length > 0 ? other.join(" ") : problemMessage(caught, FALLBACK));
    }
  }

  return (
    <form onSubmit={onSubmit} className="flex flex-col gap-4" noValidate>
      <JsonField
        label="Input"
        value={inputText}
        onChange={(value) => {
          setInputText(value);
          onDirty();
        }}
        error={errors.input}
      />
      <JsonField
        label="Expected output"
        value={expectedText}
        onChange={(value) => {
          setExpectedText(value);
          onDirty();
        }}
        error={errors.expected_output}
      />
      <JsonField
        label="Metadata"
        value={metadataText}
        onChange={(value) => {
          setMetadataText(value);
          onDirty();
        }}
        error={errors.metadata}
      />
      <FormError message={formError} />
      <DialogFooter>
        <SubmitButton pending={add.isPending}>Add item</SubmitButton>
      </DialogFooter>
    </form>
  );
}
