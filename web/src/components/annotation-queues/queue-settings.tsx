import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  annotationQueueItemsKey,
  annotationQueueKey,
  annotationQueuesKey,
} from "@/api/queries-annotation-queues";
import type { Schema } from "@/api/types";
import { QueueDialog } from "@/components/annotation-queues/queue-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { Markdown } from "@/components/payload/markdown";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { targetLabel } from "@/lib/annotation-queues";
import { canManageQueues } from "@/lib/roles";

const EDIT_REASON = "Only members can change queues";
const FALLBACK_DELETE = "The queue could not be deleted";

/** The queue's fields, with the edit and delete actions. Delete needs the queue's name typed. */
export function QueueSettings({ queue }: { queue: Schema<"AnnotationQueue"> }) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const canManage = canManageQueues(data.role);
  const reason = canManage ? undefined : EDIT_REASON;
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/annotation-queues/{id}");

  function onDelete() {
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: queue.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: annotationQueuesKey(org, project) });
          // The queue is gone: drop its own cached reads rather than let them refetch.
          queryClient.removeQueries({ queryKey: annotationQueueKey(org, project, queue.id) });
          queryClient.removeQueries({ queryKey: annotationQueueItemsKey(org, project, queue.id) });
          void navigate({
            to: "/$org/$project/annotation-queues",
            params: { org, project },
            search: {},
          });
        },
      },
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <dl className="grid grid-cols-[10rem_1fr] gap-x-4 gap-y-3 text-body">
        <dt className="text-muted-foreground">Name</dt>
        <dd>{queue.name}</dd>
        <dt className="text-muted-foreground">Kind</dt>
        <dd>{targetLabel(queue.target)}</dd>
        <dt className="text-muted-foreground">Description</dt>
        <dd>{queue.description ? queue.description : "None"}</dd>
        <dt className="text-muted-foreground">Instructions</dt>
        <dd>{queue.instructions ? <Markdown text={queue.instructions} /> : "None"}</dd>
        <dt className="text-muted-foreground">Score definitions</dt>
        <dd>{`${queue.definition_ids.length} in review order`}</dd>
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
            Delete queue
          </Button>
        </DisabledReason>
      </div>
      <QueueDialog open={editing} onOpenChange={setEditing} queue={queue} />
      <TypeToConfirmDialog
        open={deleting}
        onOpenChange={(open) => {
          setDeleting(open);
          if (!open) {
            remove.reset();
          }
        }}
        title="Delete queue"
        description="Its items are removed. Scores stay."
        expected={queue.name}
        inputLabel={`Type the queue name "${queue.name}" to confirm`}
        confirmLabel="Delete queue"
        pending={remove.isPending}
        error={remove.error ? problemMessage(remove.error, FALLBACK_DELETE) : undefined}
        onConfirm={onDelete}
      />
    </div>
  );
}
