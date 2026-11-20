import { useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { annotationQueueWriteKeys } from "@/api/queries-annotation-queues";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
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
import { Textarea } from "@/components/ui/textarea";
import { useGoToNext } from "./next-item";

/** The reason's limit, in characters, as the server counts it (D-AQ-5). */
const REASON_MAX = 1000;

/**
 * Skips the item with an optional reason. Saves nothing from the form, then moves on as Next does.
 */
export function SkipDialog({
  open,
  onOpenChange,
  queueId,
  itemId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  queueId: string;
  itemId: string;
}) {
  const { org, project, ref } = useProject();
  const queryClient = useQueryClient();
  const skip = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/skip",
  );
  const goToNext = useGoToNext(queueId);
  const id = useId();
  const [reason, setReason] = useState("");
  const [error, setError] = useState<string | undefined>();

  async function submit() {
    setError(undefined);
    try {
      await skip.mutateAsync({
        params: { path: { project: ref, id: queueId, item_id: itemId } },
        body: { reason: reason.trim() },
      });
    } catch (caught) {
      setError(problemMessage(caught, "Could not skip this item."));
      return;
    }
    await Promise.all(
      annotationQueueWriteKeys(org, project, queueId).map((key) =>
        queryClient.invalidateQueries({ queryKey: key }),
      ),
    );
    onOpenChange(false);
    await goToNext(itemId);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && skip.isPending) {
          return;
        }
        onOpenChange(next);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Skip this item</DialogTitle>
          <DialogDescription>
            The item is marked skipped. Unsaved annotations are not saved.
          </DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-1">
          <Label htmlFor={id}>Reason (optional)</Label>
          <Textarea
            id={id}
            maxLength={REASON_MAX}
            value={reason}
            onChange={(event) => {
              setReason(event.target.value);
            }}
          />
        </div>
        <FormError message={error} />
        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => {
              onOpenChange(false);
            }}
          >
            Cancel
          </Button>
          <Button
            type="button"
            disabled={skip.isPending}
            onClick={() => {
              void submit();
            }}
          >
            Skip and next
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
