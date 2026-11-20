import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetsInit, datasetsKey, datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { shortId } from "@/lib/format";
import { useBackState } from "@/lib/list-return";
import { canManageDatasets, type OrgRole } from "@/lib/roles";

/** The most traces one bulk add copies (D-J-25). */
export const ADD_TRACES_MAX = 100;

type DatasetRef = { id: string; name: string };
type FormField = "dataset" | "name";

/** Select value of the "New dataset…" option. Dataset ids are UUIDs, so it cannot clash. */
const NEW_DATASET = "new";
const DUPLICATE_NAME = "a dataset with this name already exists";

/** Why the traces list cannot add `count` checked traces to a dataset; undefined when it can. */
export function addTracesReason(role: OrgRole | undefined, count: number): string | undefined {
  if (!canManageDatasets(role)) {
    return "Only members can add to datasets";
  }
  if (count > ADD_TRACES_MAX) {
    return `At most ${ADD_TRACES_MAX} traces at once`;
  }
  return undefined;
}

function useCreateDataset() {
  return $api.useMutation("post", "/api/v1/projects/{project}/datasets");
}

function useAddFromTraces() {
  return $api.useMutation(
    "post",
    "/api/v1/projects/{project}/datasets/{dataset}/items/from-traces",
  );
}

export interface AddTracesToDatasetDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The checked traces. Each is copied from its root span (D-J-25). */
  traceIds: readonly string[];
  /** Called once the traces are added, so the list can clear its selection. */
  onAdded: () => void;
}

/** Copies the checked traces of the traces list into a dataset as one version (D-J-25). */
export function AddTracesToDatasetDialog({
  open,
  onOpenChange,
  traceIds,
  onAdded,
}: AddTracesToDatasetDialogProps) {
  const { org, project } = useProject();
  const addFromTraces = useAddFromTraces();
  const count = traceIds.length;

  return (
    <DatasetTargetDialog
      open={open}
      onOpenChange={onOpenChange}
      title={`Add ${count} ${count === 1 ? "trace" : "traces"} to a dataset`}
      description="Copies each trace's root input and output into a new dataset item, all in one new version."
      adding={addFromTraces.isPending}
      add={(datasetId) =>
        addFromTraces.mutateAsync({
          params: { path: { project: projectRef(org, project), dataset: datasetId } },
          body: { items: traceIds.map((trace_id) => ({ trace_id })) },
        })
      }
      onAdded={onAdded}
    />
  );
}

export interface AddIssueExamplesDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The issue whose newest occurrence traces are copied. */
  issueId: string;
}

/** Copies the newest occurrence traces of an issue into a dataset as one version (D-IC-22). */
export function AddIssueExamplesDialog({
  open,
  onOpenChange,
  issueId,
}: AddIssueExamplesDialogProps) {
  const { org, project } = useProject();
  const addExamples = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/issues/{id}/dataset-items",
  );

  return (
    <DatasetTargetDialog
      open={open}
      onOpenChange={onOpenChange}
      title="Add examples to a dataset"
      description="Copies the input and output of the issue's newest occurrences (up to 100 traces, one item each) into a dataset as one new version."
      adding={addExamples.isPending}
      add={(datasetId) =>
        addExamples.mutateAsync({
          params: { path: { project: projectRef(org, project), id: issueId } },
          body: { dataset: datasetId },
        })
      }
    />
  );
}

interface DatasetTargetDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description: string;
  /** Adds to the dataset; resolves with J's result. */
  add: (datasetId: string) => Promise<Schema<"FromTracesResult">>;
  /** True while that add request runs (the caller's mutation `isPending`). */
  adding: boolean;
  /** Optional: the traces list clears its selection; the issue dialog passes none. */
  onAdded?: () => void;
}

/** The dialog shell and the dataset form shared by the traces list and the issue page. */
function DatasetTargetDialog({
  open,
  onOpenChange,
  title,
  description,
  add,
  adding,
  onAdded,
}: DatasetTargetDialogProps) {
  const createDataset = useCreateDataset();
  const busy = createDataset.isPending || adding;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && busy) {
          return;
        }
        onOpenChange(next);
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onInteractOutside={(event) => {
          if (busy) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (busy) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        {open ? (
          <AddTracesForm
            createDataset={createDataset}
            add={add}
            adding={adding}
            onAdded={onAdded}
            onClose={() => {
              onOpenChange(false);
            }}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function AddTracesForm({
  createDataset,
  add,
  adding,
  onAdded,
  onClose,
}: {
  createDataset: ReturnType<typeof useCreateDataset>;
  add: (datasetId: string) => Promise<Schema<"FromTracesResult">>;
  adding: boolean;
  onAdded?: () => void;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const ref = projectRef(org, project);
  const queryClient = useQueryClient();
  const backState = useBackState("Traces");

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

  const [errors, setErrors] = useState<Partial<Record<FormField, string>>>({});
  const [formError, setFormError] = useState<string | undefined>();
  const [result, setResult] = useState<
    { dataset: DatasetRef; added: number; skipped: Schema<"SkippedTrace">[] } | undefined
  >();

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

  /** Field errors of a 400 (one per trace that was not copied), else the problem's detail. */
  function addMessage(caught: unknown): string {
    const problem = problemOf(caught);
    const messages = Object.values(fieldErrors(caught));
    if (messages.length > 0) {
      return messages.join(" ");
    }
    const fallback =
      problem?.status === 404
        ? "The dataset was not found."
        : problem?.status === 503
          ? "The server is busy. Try again."
          : "The traces could not be added.";
    return problem?.detail ?? fallback;
  }

  async function onSubmit(event: FormEvent) {
    event.preventDefault();
    setFormError(undefined);
    if (!creating && picked === undefined) {
      setErrors({ dataset: "Choose a dataset." });
      return;
    }
    const trimmed = name.trim();
    if (creating && trimmed === "") {
      setErrors({ name: "Required" });
      return;
    }
    setErrors({});

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

    try {
      const answer = await add(target.id);
      for (const key of datasetWriteKeys(org, project, target.id)) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
      onAdded?.();
      setResult({ dataset: target, added: answer.items.length, skipped: answer.skipped });
    } catch (caught) {
      setFormError(addMessage(caught));
    }
  }

  if (result !== undefined) {
    const skippedCount = result.skipped.length;
    return (
      <div className="flex flex-col gap-4">
        <p className="text-ui">
          {`Added ${result.added} ${result.added === 1 ? "item" : "items"} to ${result.dataset.name}.`}
        </p>
        {skippedCount > 0 ? (
          <div className="flex flex-col gap-2">
            <p className="text-ui font-medium">
              {`${skippedCount} ${skippedCount === 1 ? "trace was" : "traces were"} not copied`}
            </p>
            <ul className="flex flex-col gap-1 text-body">
              {result.skipped.map((trace) => (
                <li key={trace.index} className="flex gap-2">
                  <span className="font-mono">{shortId(trace.trace_id)}</span>
                  <span className="text-muted-foreground">{trace.message}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Close
          </Button>
          <Link
            to="/$org/$project/datasets/$datasetId"
            params={{ org, project, datasetId: result.dataset.id }}
            state={backState}
            className={buttonVariants({ variant: "default" })}
          >
            Open dataset
          </Link>
        </DialogFooter>
      </div>
    );
  }

  const busy = createDataset.isPending || adding;
  const message = formError ?? errors.dataset;

  return (
    <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
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
          <p className="text-body text-muted-foreground">More datasets match; refine the search.</p>
        ) : null}
        {creating ? (
          <TextField
            id="add-traces-dataset-name"
            label="Name of the new dataset"
            value={name}
            onChange={setName}
            required
            maxLength={200}
            error={errors.name}
          />
        ) : null}
      </div>

      {message !== undefined ? (
        <p role="alert" className="text-ui text-destructive">
          {message}
        </p>
      ) : null}
      <DialogFooter>
        <SubmitButton pending={busy}>Add</SubmitButton>
      </DialogFooter>
    </form>
  );
}
