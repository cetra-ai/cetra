import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  datasetItemsKey,
  datasetKey,
  datasetsKey,
  datasetVersionsKey,
} from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { DatasetDialog } from "@/components/datasets/dataset-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { JsonView } from "@/components/payload/json-view";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { canManageDatasets } from "@/lib/roles";

const EDIT_REASON = "Only members can edit datasets";

/** The dataset's settings: its name, description and schemas, and the edit and delete actions. */
export function DatasetSettings({ dataset }: { dataset: Schema<"Dataset"> }) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const canManage = canManageDatasets(data.role);
  const reason = canManage ? undefined : EDIT_REASON;
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/datasets/{dataset}");

  function onDelete() {
    remove.mutate(
      { params: { path: { project: projectRef(org, project), dataset: dataset.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: datasetsKey(org, project) });
          // The dataset is gone: drop its own cached reads rather than let them refetch.
          for (const key of [
            datasetKey(org, project, dataset.id),
            datasetItemsKey(org, project, dataset.id),
            datasetVersionsKey(org, project, dataset.id),
          ]) {
            queryClient.removeQueries({ queryKey: key });
          }
          void navigate({
            to: "/$org/$project/datasets",
            params: { org, project },
            search: { q: undefined },
          });
        },
      },
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <dl className="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-3 text-sm">
        <dt className="text-muted-foreground">Name</dt>
        <dd>{dataset.name}</dd>
        <dt className="text-muted-foreground">Description</dt>
        <dd>{dataset.description ? dataset.description : "None"}</dd>
        <dt className="text-muted-foreground">Input schema</dt>
        <dd>{schemaView(dataset.input_schema)}</dd>
        <dt className="text-muted-foreground">Expected output schema</dt>
        <dd>{schemaView(dataset.expected_schema)}</dd>
      </dl>
      <div className="flex flex-wrap gap-2">
        <DisabledReason reason={reason}>
          <Button
            type="button"
            variant="outline"
            disabled={!canManage}
            onClick={() => {
              setEditing(true);
            }}
          >
            Edit
          </Button>
        </DisabledReason>
        <DisabledReason reason={reason}>
          <Button
            type="button"
            variant="destructive"
            disabled={!canManage}
            onClick={() => {
              setDeleting(true);
            }}
          >
            Delete dataset
          </Button>
        </DisabledReason>
      </div>
      <DatasetDialog open={editing} onOpenChange={setEditing} dataset={dataset} />
      <TypeToConfirmDialog
        open={deleting}
        onOpenChange={(open) => {
          setDeleting(open);
          if (!open) {
            remove.reset();
          }
        }}
        title="Delete dataset"
        description="Deletes the dataset and all its versions. Experiments keep their results and show the dataset as deleted."
        expected={dataset.name}
        inputLabel={`Type the dataset name "${dataset.name}" to confirm`}
        confirmLabel="Delete dataset"
        pending={remove.isPending}
        error={
          remove.error
            ? problemMessage(remove.error, "The dataset could not be deleted")
            : undefined
        }
        onConfirm={onDelete}
      />
    </div>
  );
}

function schemaView(schema: Schema<"Dataset">["input_schema"]) {
  return schema === undefined ? "None" : <JsonView value={schema} />;
}
