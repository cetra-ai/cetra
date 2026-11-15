import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { spanDetailQuery } from "@/api/queries";
import { datasetsInit, datasetsKey, datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { useProject } from "@/components/shell/project-context";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField, payloadJson, prettyJson } from "@/lib/datasets";
import { canManageDatasets } from "@/lib/roles";

type From = "trace" | "span";
type FormField = "dataset" | "name" | "input" | "expected_output" | "metadata";
type Metadata = Schema<"FromTraceRequest">["metadata"];
type DatasetRef = { id: string; name: string };

/** Select value of the "New dataset…" option. Dataset ids are UUIDs, so it cannot clash. */
const NEW_DATASET = "new";
const ITEM_FIELDS = ["input", "expected_output", "metadata"] as const;
const NOT_FOUND =
  "The trace was not found. It may have been deleted or be outside your plan's data window.";
const DUPLICATE_NAME = "a dataset with this name already exists";

/** The trace and the span the trace view has selected, and its root span. */
export interface AddToDatasetTarget {
  traceId: string;
  /** The trace's root span (a span with no parent), when the loaded trace has one. */
  rootSpanId?: string;
  /** The span the trace view has selected. */
  spanId?: string;
  /** Whether the selected span is the root. */
  selectedIsRoot: boolean;
}

/** Copies a trace or a span into a dataset as one new item (D-J-8). */
export function AddToDatasetButton(props: AddToDatasetTarget) {
  const { data } = useProject();
  const [open, setOpen] = useState(false);
  const canAdd = canManageDatasets(data.role);

  return (
    <>
      <DisabledReason reason={canAdd ? undefined : "Only members can add to datasets"}>
        <Button
          type="button"
          variant="outline"
          size="xs"
          disabled={!canAdd}
          onClick={() => {
            setOpen(true);
          }}
        >
          Add to dataset
        </Button>
      </DisabledReason>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Add to dataset</DialogTitle>
            <DialogDescription>
              Copies the input and output into a new dataset item. Edit them before saving.
            </DialogDescription>
          </DialogHeader>
          {open ? (
            <AddToDatasetForm
              {...props}
              onClose={() => {
                setOpen(false);
              }}
            />
          ) : null}
        </DialogContent>
      </Dialog>
    </>
  );
}

function AddToDatasetForm({
  traceId,
  rootSpanId,
  spanId,
  selectedIsRoot,
  onClose,
}: AddToDatasetTarget & { onClose: () => void }) {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  const queryClient = useQueryClient();

  // A span can be the source only when the trace view selected one that is not the root.
  const spanTargetable = spanId !== undefined && !selectedIsRoot;
  const [pickedFrom, setPickedFrom] = useState<From>("span");
  const from: From = spanTargetable ? pickedFrom : "trace";
  // The root's span id is sent for "Trace", so the stored source span is the one prefilled.
  const sourceSpanId = from === "span" ? spanId : rootSpanId;

  const source = useQuery({
    ...spanDetailQuery(org, project, traceId, sourceSpanId ?? ""),
    enabled: sourceSpanId !== undefined,
  });
  const sourceSpan = source.data;
  const loading = sourceSpanId !== undefined && source.isPending;

  const [inputText, setInputText] = useState("");
  const [expectedText, setExpectedText] = useState("");
  const [metadataText, setMetadataText] = useState("{}");
  // A field the user typed in keeps its text when the source changes.
  const edited = useRef({ input: false, expected: false });

  useEffect(() => {
    if (sourceSpanId !== undefined && sourceSpan === undefined) {
      return;
    }
    if (!edited.current.input) {
      setInputText(prettyJson(payloadJson(sourceSpan?.input)));
    }
    if (!edited.current.expected) {
      setExpectedText(prettyJson(payloadJson(sourceSpan?.output)));
    }
  }, [sourceSpanId, sourceSpan]);

  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  useEffect(() => {
    const timer = setTimeout(() => {
      setQuery(search.trim());
    }, 300);
    return () => {
      clearTimeout(timer);
    };
  }, [search]);
  const datasets = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/datasets",
    datasetsInit(org, project, query),
  );
  const [creating, setCreating] = useState(false);
  // The dataset chosen from the list, or created here. Kept so it stays shown when the search hides it.
  const [picked, setPicked] = useState<DatasetRef | undefined>();
  const [name, setName] = useState("");
  const listed = datasets.data?.data ?? [];
  const options =
    picked !== undefined && !listed.some((dataset) => dataset.id === picked.id)
      ? [...listed, picked]
      : listed;

  const createDataset = $api.useMutation("post", "/api/v1/projects/{project}/datasets");
  const addFromTrace = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/datasets/{dataset}/items/from-trace",
  );
  const [errors, setErrors] = useState<Partial<Record<FormField, string>>>({});
  const [formError, setFormError] = useState<string | undefined>();
  const [added, setAdded] = useState<DatasetRef | undefined>();

  function pickDataset(value: string) {
    setErrors({});
    setFormError(undefined);
    if (value === NEW_DATASET) {
      setCreating(true);
      return;
    }
    const next = listed.find((dataset) => dataset.id === value);
    if (next) {
      setPicked({ id: next.id, name: next.name });
      setCreating(false);
    }
  }

  function showCreateError(caught: unknown) {
    const problem = problemOf(caught);
    const fields = fieldErrors(caught);
    if (fields.name !== undefined) {
      setErrors({ name: fields.name });
    } else if (problem?.status === 409 && problem.detail === DUPLICATE_NAME) {
      setErrors({ name: "A dataset with this name exists" });
    } else {
      setFormError(problemMessage(caught, "The dataset could not be created"));
    }
  }

  function showItemError(caught: unknown) {
    const problem = problemOf(caught);
    if (problem?.status === 404) {
      setFormError(NOT_FOUND);
      return;
    }
    const placed: Partial<Record<FormField, string>> = {};
    for (const [field, message] of Object.entries(fieldErrors(caught))) {
      const known = ITEM_FIELDS.find((candidate) => candidate === field);
      if (known) {
        placed[known] = message;
      }
    }
    if (Object.keys(placed).length > 0) {
      setErrors(placed);
      return;
    }
    const fallback =
      problem?.status === 409 ? "The dataset is full" : "The item could not be added";
    setFormError(problemMessage(caught, fallback));
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    if (!creating && picked === undefined) {
      setErrors({ dataset: "Choose a dataset." });
      return;
    }
    const input = parseJsonField(inputText, { required: true });
    const expected = parseJsonField(expectedText, { required: false });
    const metadata = parseJsonField(metadataText, { required: false, object: true });
    const trimmed = name.trim();
    const nextErrors: Partial<Record<FormField, string>> = {};
    if (creating && trimmed === "") {
      nextErrors.name = "Required";
    }
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
    if (Object.keys(nextErrors).length > 0 || !input.ok || !expected.ok || !metadata.ok) {
      return;
    }

    let target = picked;
    if (creating) {
      try {
        const created = await createDataset.mutateAsync({
          params: { path: { project: ref } },
          body: { name: trimmed },
        });
        target = { id: created.id, name: created.name };
        setPicked(target);
        setCreating(false);
        void queryClient.invalidateQueries({ queryKey: datasetsKey(org, project) });
      } catch (caught) {
        showCreateError(caught);
        return;
      }
    }
    if (target === undefined) {
      return;
    }

    const item: Schema<"FromTraceRequest"> = { trace_id: traceId, input: input.value };
    if (sourceSpanId !== undefined) {
      item.span_id = sourceSpanId;
    }
    if (expected.value !== undefined) {
      item.expected_output = expected.value;
    }
    if (metadata.value !== undefined) {
      item.metadata = metadata.value as Metadata;
    }
    try {
      await addFromTrace.mutateAsync({
        params: { path: { project: ref, dataset: target.id } },
        body: item,
      });
      for (const key of datasetWriteKeys(org, project, target.id)) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
      setAdded(target);
    } catch (caught) {
      showItemError(caught);
    }
  }

  if (added !== undefined) {
    return (
      <div className="flex flex-col gap-4">
        <p className="text-sm">{`Added to ${added.name}.`}</p>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Close
          </Button>
          <Link
            to="/$org/$project/datasets/$datasetId"
            params={{ org, project, datasetId: added.id }}
            className={buttonVariants({ variant: "default" })}
          >
            Open dataset
          </Link>
        </DialogFooter>
      </div>
    );
  }

  const busy = createDataset.isPending || addFromTrace.isPending;
  const loadMessage = source.isError ? "Could not read the span." : undefined;
  const noRootMessage =
    from === "trace" && rootSpanId === undefined
      ? "This trace has no root span; type the input."
      : undefined;
  const message = loadMessage ?? formError ?? errors.dataset;

  return (
    <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
      {spanTargetable ? (
        <div className="flex flex-col gap-2">
          <span className="text-sm leading-none font-medium">From</span>
          <RadioGroup
            aria-label="From"
            value={from}
            onValueChange={(next) => {
              setPickedFrom(next === "span" ? "span" : "trace");
            }}
            className="flex-row gap-4"
          >
            <div className="flex items-center gap-2">
              <RadioGroupItem value="trace" id="add-from-trace" />
              <Label htmlFor="add-from-trace">Trace</Label>
            </div>
            <div className="flex items-center gap-2">
              <RadioGroupItem value="span" id="add-from-span" />
              <Label htmlFor="add-from-span">Selected span</Label>
            </div>
          </RadioGroup>
        </div>
      ) : null}

      <div className="flex flex-col gap-2">
        <Input
          aria-label="Search datasets"
          placeholder="Search by name"
          value={search}
          onChange={(event) => {
            setSearch(event.target.value);
          }}
        />
        <Select value={creating ? NEW_DATASET : (picked?.id ?? "")} onValueChange={pickDataset}>
          <SelectTrigger
            aria-label="Dataset"
            className="w-full"
            aria-invalid={errors.dataset ? true : undefined}
          >
            <SelectValue placeholder="Choose a dataset" />
          </SelectTrigger>
          <SelectContent>
            {options.map((dataset) => (
              <SelectItem key={dataset.id} value={dataset.id}>
                {dataset.name}
              </SelectItem>
            ))}
            <SelectItem value={NEW_DATASET}>New dataset…</SelectItem>
          </SelectContent>
        </Select>
        {datasets.data?.next_cursor ? (
          <p className="text-xs text-muted-foreground">More datasets match; refine the search.</p>
        ) : null}
        {creating ? (
          <TextField
            id="add-to-dataset-name"
            label="Name of the new dataset"
            value={name}
            onChange={setName}
            required
            maxLength={200}
            error={errors.name}
          />
        ) : null}
      </div>

      {noRootMessage ? <p className="text-sm text-muted-foreground">{noRootMessage}</p> : null}
      <JsonField
        label="Input"
        value={inputText}
        disabled={loading}
        onChange={(value) => {
          edited.current.input = true;
          setInputText(value);
        }}
        error={errors.input}
      />
      <JsonField
        label="Expected output"
        value={expectedText}
        disabled={loading}
        onChange={(value) => {
          edited.current.expected = true;
          setExpectedText(value);
        }}
        error={errors.expected_output}
      />
      <JsonField
        label="Metadata"
        value={metadataText}
        disabled={loading}
        onChange={setMetadataText}
        error={errors.metadata}
      />

      <FormError message={message} />
      <DialogFooter>
        <SubmitButton pending={busy} disabled={loading}>
          Add item
        </SubmitButton>
      </DialogFooter>
    </form>
  );
}

function JsonField({
  label,
  value,
  disabled,
  onChange,
  error,
}: {
  label: string;
  value: string;
  disabled: boolean;
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
        disabled={disabled}
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
