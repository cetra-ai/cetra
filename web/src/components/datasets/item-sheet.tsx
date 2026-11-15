import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { IdText } from "@/components/id-text";
import { JsonView } from "@/components/payload/json-view";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField, prettyJson, sameJson } from "@/lib/datasets";

const FALLBACK_SAVE = "The item could not be saved";
const FALLBACK_DELETE = "The item could not be deleted";
const PLACED_FIELDS = ["input", "expected_output", "metadata"] as const;

type ItemField = (typeof PLACED_FIELDS)[number];
type Metadata = Schema<"UpdateItemRequest">["metadata"];

/**
 * Sends the item's changes, with only the changed fields. The item sheet shows one item
 * at a time; the editable view is only offered on the latest version (the page decides).
 */
export function ItemSheet({
  datasetId,
  item,
  editable,
  onOpenChange,
}: {
  datasetId: string;
  item: Schema<"DatasetItem"> | undefined;
  /** Whether the item can be edited: the latest version, and the user may manage datasets. */
  editable: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <Sheet open={item !== undefined} onOpenChange={onOpenChange}>
      <SheetContent className="overflow-y-auto sm:max-w-2xl">
        <SheetHeader>
          <SheetTitle>Item</SheetTitle>
          {item ? <IdText value={item.id} kind="item" /> : null}
        </SheetHeader>
        {item ? (
          <ItemBody
            key={item.id}
            datasetId={datasetId}
            item={item}
            editable={editable}
            onOpenChange={onOpenChange}
          />
        ) : null}
      </SheetContent>
    </Sheet>
  );
}

function ItemBody({
  datasetId,
  item,
  editable,
  onOpenChange,
}: {
  datasetId: string;
  item: Schema<"DatasetItem">;
  editable: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const [inputText, setInputText] = useState(prettyJson(item.input));
  const [expectedText, setExpectedText] = useState(prettyJson(item.expected_output));
  const [metadataText, setMetadataText] = useState(prettyJson(item.metadata));
  const [errors, setErrors] = useState<Partial<Record<ItemField, string>>>({});
  const [formError, setFormError] = useState<string | undefined>();
  const [confirming, setConfirming] = useState(false);
  const patch = $api.useMutation(
    "patch",
    "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
  );
  const remove = $api.useMutation(
    "delete",
    "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
  );
  const pending = patch.isPending || remove.isPending;

  function invalidate() {
    for (const key of datasetWriteKeys(org, project, datasetId)) {
      void queryClient.invalidateQueries({ queryKey: key });
    }
  }

  function onSave(event: FormEvent) {
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

    // Only the changed fields are sent. An expected output that is now empty is sent as null.
    const body: Schema<"UpdateItemRequest"> = {};
    if (!sameJson(input.value, item.input)) {
      body.input = input.value;
    }
    if (!sameJson(expected.value, item.expected_output)) {
      body.expected_output = expected.value === undefined ? null : expected.value;
    }
    const nextMetadata = metadata.value ?? {};
    if (!sameJson(nextMetadata, item.metadata)) {
      body.metadata = nextMetadata as Metadata;
    }
    if (Object.keys(body).length === 0) {
      onOpenChange(false);
      return;
    }
    patch.mutate(
      {
        params: {
          path: { project: projectRef(org, project), dataset: datasetId, item_id: item.id },
        },
        body,
      },
      {
        onSuccess: () => {
          invalidate();
          onOpenChange(false);
        },
        onError: showSaveErrors,
      },
    );
  }

  function showSaveErrors(caught: unknown) {
    const placed: Partial<Record<ItemField, string>> = {};
    const other: string[] = [];
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      const name = PLACED_FIELDS.find(
        (candidate) =>
          field === candidate ||
          field.startsWith(`${candidate}.`) ||
          field.startsWith(`${candidate}[`),
      );
      if (name) {
        placed[name] = message;
      } else {
        other.push(message);
      }
    }
    setErrors(placed);
    if (Object.keys(placed).length === 0 || other.length > 0) {
      setFormError(other.length > 0 ? other.join(" ") : problemMessage(caught, FALLBACK_SAVE));
    }
  }

  function onDelete() {
    remove.mutate(
      {
        params: {
          path: { project: projectRef(org, project), dataset: datasetId, item_id: item.id },
        },
      },
      {
        onSuccess: () => {
          invalidate();
          setConfirming(false);
          onOpenChange(false);
        },
      },
    );
  }

  return (
    <div className="flex flex-col gap-6">
      {editable ? (
        <form onSubmit={onSave} className="flex flex-col gap-4" noValidate>
          <JsonArea label="Input" value={inputText} onChange={setInputText} error={errors.input} />
          <JsonArea
            label="Expected output"
            value={expectedText}
            onChange={setExpectedText}
            error={errors.expected_output}
          />
          <JsonArea
            label="Metadata"
            value={metadataText}
            onChange={setMetadataText}
            error={errors.metadata}
          />
          <FormError message={formError} />
          <div className="flex flex-wrap gap-2">
            <SubmitButton pending={pending}>Save</SubmitButton>
            <Button
              type="button"
              variant="outline"
              disabled={pending}
              onClick={() => {
                setConfirming(true);
              }}
            >
              Delete item
            </Button>
          </div>
        </form>
      ) : (
        <>
          <JsonSection title="Input" value={item.input} />
          <JsonSection title="Expected output" value={item.expected_output} />
          <JsonSection title="Metadata" value={item.metadata} />
        </>
      )}
      <dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-2 text-sm">
        <dt className="text-muted-foreground">Added in</dt>
        <dd>{`v${item.version}`}</dd>
        <dt className="text-muted-foreground">Revision</dt>
        <dd>
          <IdText value={item.revision_id} kind="revision" />
        </dd>
        <dt className="text-muted-foreground">Source</dt>
        <dd>
          {item.source_trace_id !== undefined ? (
            <Link
              to="/$org/$project/traces/$traceId"
              params={{ org, project, traceId: item.source_trace_id }}
              search={item.source_span_id !== undefined ? { span: item.source_span_id } : {}}
              className="underline-offset-4 hover:underline"
            >
              trace
            </Link>
          ) : (
            "—"
          )}
        </dd>
      </dl>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Delete item"
        description="The item leaves the dataset in a new version. Older versions keep it."
        confirmLabel="Delete item"
        destructive
        pending={remove.isPending}
        error={remove.error ? problemMessage(remove.error, FALLBACK_DELETE) : undefined}
        onConfirm={onDelete}
      />
    </div>
  );
}

function JsonArea({
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
        rows={6}
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

function JsonSection({ title, value }: { title: string; value: unknown }) {
  return (
    <section className="flex flex-col gap-2">
      <h3 className="text-sm font-medium">{title}</h3>
      {value === undefined ? (
        <p className="text-sm text-muted-foreground">None</p>
      ) : (
        <JsonView value={value} />
      )}
    </section>
  );
}
