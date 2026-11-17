import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ChevronLeft, ChevronRight, Pencil, X } from "lucide-react";
import { type FormEvent, type ReactNode, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { SourceLink } from "@/components/datasets/item-columns";
import { type ItemField, JsonField, splitItemErrors } from "@/components/datasets/item-fields";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { IdText } from "@/components/id-text";
import { PayloadView } from "@/components/payload/payload-view";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { itemShortId, parseJsonField, prettyJson, sameJson } from "@/lib/datasets";

const FALLBACK_SAVE = "The item could not be saved";
const FALLBACK_DELETE = "The item could not be deleted";

type Item = Schema<"DatasetItem">;
type Metadata = Schema<"UpdateItemRequest">["metadata"];

function usePatchItem() {
  return $api.useMutation("patch", "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}");
}

function useDeleteItem() {
  return $api.useMutation(
    "delete",
    "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
  );
}

/**
 * A raw JSON value as a payload: a string is text (Markdown in Pretty mode), any other value is
 * JSON. Nothing (undefined, or empty metadata) is undefined, which shows "—".
 */
export function payloadOf(value: unknown): Schema<"Payload"> | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value === "string") {
    return { format: "text", value };
  }
  return { format: "json", value };
}

/** The panel's metadata payload: nothing when the item has no metadata. */
function metadataPayload(metadata: Item["metadata"]): Schema<"Payload"> | undefined {
  return Object.keys(metadata).length === 0 ? undefined : payloadOf(metadata);
}

/**
 * One item beside the items list: its input, expected output and metadata, its source, and the
 * editable form on the latest version. Saving sends only the changed fields and then shows the
 * new revision. `onDirtyChange` tells the page the form has typed-in changes (Escape waits).
 */
export function ItemPanel({
  datasetId,
  item,
  editable,
  previous,
  next,
  onMove,
  onClose,
  onDirtyChange,
  layout,
}: {
  datasetId: string;
  item: Item;
  /** Whether the item can be edited: the latest version, and the user may manage datasets. */
  editable: boolean;
  previous?: Item;
  next?: Item;
  onMove: (item: Item) => void;
  onClose: () => void;
  onDirtyChange: (dirty: boolean) => void;
  layout?: ReactNode;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const patch = usePatchItem();
  const remove = useDeleteItem();
  const [editing, setEditing] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [inputText, setInputText] = useState(prettyJson(item.input));
  const [expectedText, setExpectedText] = useState(prettyJson(item.expected_output));
  const [metadataText, setMetadataText] = useState(prettyJson(item.metadata));
  const [errors, setErrors] = useState<Partial<Record<ItemField, string>>>({});
  const [formError, setFormError] = useState<string | undefined>();
  const [confirming, setConfirming] = useState(false);
  const pending = patch.isPending || remove.isPending;
  // The page's Escape waits while the form is dirty; a closed panel must not leave that set.
  useEffect(() => () => onDirtyChange(false), [onDirtyChange]);
  const ref = {
    project: projectRef(org, project),
    dataset: datasetId,
    item_id: item.id,
  };

  function invalidate() {
    for (const key of datasetWriteKeys(org, project, datasetId)) {
      void queryClient.invalidateQueries({ queryKey: key });
    }
  }

  function edited() {
    setDirty(true);
    onDirtyChange(true);
  }

  function leaveEditing() {
    setEditing(false);
    setDirty(false);
    onDirtyChange(false);
    patch.reset();
    setFormError(undefined);
    setErrors({});
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
      leaveEditing();
      return;
    }
    patch.mutate(
      { params: { path: ref }, body },
      {
        onSuccess: () => {
          invalidate();
          leaveEditing();
        },
        onError: showSaveErrors,
      },
    );
  }

  function showSaveErrors(caught: unknown) {
    const { placed, other } = splitItemErrors(caught);
    setErrors(placed);
    if (Object.keys(placed).length === 0 || other.length > 0) {
      setFormError(other.length > 0 ? other.join(" ") : problemMessage(caught, FALLBACK_SAVE));
    }
  }

  function onDelete() {
    remove.mutate(
      { params: { path: ref } },
      {
        onSuccess: () => {
          invalidate();
          setConfirming(false);
          onDirtyChange(false);
          onClose();
        },
      },
    );
  }

  return (
    <section aria-label="Item" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-body">
        <IdText value={item.id} kind="item" shown={itemShortId(item.id)} />
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-label="Previous item"
            disabled={previous === undefined || dirty || pending}
            onClick={() => {
              if (previous) {
                onMove(previous);
              }
            }}
          >
            <ChevronLeft />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-label="Next item"
            disabled={next === undefined || dirty || pending}
            onClick={() => {
              if (next) {
                onMove(next);
              }
            }}
          >
            <ChevronRight />
          </Button>
          {editable && !editing ? (
            <Button
              type="button"
              variant="outline"
              size="xs"
              onClick={() => {
                setEditing(true);
              }}
            >
              <Pencil />
              Edit item
            </Button>
          ) : null}
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto p-3 text-body">
        {editing ? (
          <form onSubmit={onSave} className="flex flex-col gap-4" noValidate>
            <JsonField
              label="Input"
              value={inputText}
              rows={6}
              onChange={(value) => {
                setInputText(value);
                edited();
              }}
              error={errors.input}
            />
            <JsonField
              label="Expected output"
              value={expectedText}
              rows={6}
              onChange={(value) => {
                setExpectedText(value);
                edited();
              }}
              error={errors.expected_output}
            />
            <JsonField
              label="Metadata"
              value={metadataText}
              rows={6}
              onChange={(value) => {
                setMetadataText(value);
                edited();
              }}
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
                  leaveEditing();
                }}
              >
                Cancel
              </Button>
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
            <PayloadView payload={payloadOf(item.input)} label="Input" />
            <PayloadView payload={payloadOf(item.expected_output)} label="Expected output" />
            <PayloadView payload={metadataPayload(item.metadata)} label="Metadata" />
          </>
        )}
        <section aria-label="Source" className="flex flex-col gap-2">
          <h3 className="text-body font-medium">Source</h3>
          <dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-2 text-body">
            <dt className="text-muted-foreground">Added in</dt>
            <dd className="num">{`v${item.version}`}</dd>
            <dt className="text-muted-foreground">Revision</dt>
            <dd>
              <IdText
                value={item.revision_id}
                kind="revision"
                shown={itemShortId(item.revision_id)}
              />
            </dd>
            <dt className="text-muted-foreground">Trace</dt>
            <dd className="flex flex-wrap items-center gap-2">
              {item.source_trace_id !== undefined ? (
                <>
                  <SourceLink traceId={item.source_trace_id} spanId={item.source_span_id} />
                  <Button variant="outline" size="xs" asChild>
                    <Link
                      to="/$org/$project/traces/$traceId"
                      params={{ org, project, traceId: item.source_trace_id }}
                      search={
                        item.source_span_id !== undefined ? { span: item.source_span_id } : {}
                      }
                    >
                      Open trace
                    </Link>
                  </Button>
                </>
              ) : (
                <span className="text-muted-foreground">—</span>
              )}
            </dd>
          </dl>
        </section>
      </div>
      <ConfirmDialog
        open={confirming}
        onOpenChange={(open) => {
          setConfirming(open);
          if (!open) {
            remove.reset();
          }
        }}
        title="Delete item"
        description="The item leaves the dataset in a new version. Older versions keep it."
        confirmLabel="Delete item"
        destructive
        pending={remove.isPending}
        error={remove.error ? problemMessage(remove.error, FALLBACK_DELETE) : undefined}
        onConfirm={onDelete}
      />
    </section>
  );
}
